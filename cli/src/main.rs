mod progress;
use progress::ProgressMonitor;

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, ErrorKind};
use std::path::Path;
use std::path::PathBuf;

use progress::fmt_size;

use clap::{Parser, Subcommand};
use color_eyre::eyre::{ContextCompat, WrapErr, eyre};
use structured_zstd::encoding::CompressionLevel;
use tracing::info;
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

// TODO: implement a dictionary creation command, and a command for benchmarking
#[derive(Subcommand)]
enum Commands {
    /// Compress a single file. If no output file is specified,
    /// output will be written to <INPUT_FILE>.zst
    Compress {
        /// File to compress
        input_file: PathBuf,
        /// Where the compressed file is written
        /// [default: <INPUT_FILE>.zst]
        output_file: Option<PathBuf>,
        /// How thoroughly the file should be compressed. A higher level will take
        /// more time to compress but result in a smaller file, and vice versa.
        ///
        /// - 0: Uncompressed
        /// - 1: Fastest
        /// - 2: Default
        /// - 3: Better (lazy2, ~zstd level 7)
        /// - 4: Best  (deep lazy2, ~zstd level 11)
        #[arg(
            short,
            long,
            value_name = "COMPRESSION_LEVEL",
            default_value_t = 2,
            value_parser = clap::value_parser!(u8).range(0..=4),
            verbatim_doc_comment
        )]
        level: u8,
    },
    Decompress {
        /// .zst archive to decompress
        input_file: PathBuf,
        /// Where the compressed file is written
        /// [default: <ARCHIVE_NAME>]
        output_file: Option<PathBuf>,
    },
}

fn main() -> color_eyre::Result<()> {
    // Process CLI arguments
    let cli = Cli::parse();
    // Initialize logging (with indicatif integration)
    let indicatif_layer = IndicatifLayer::new();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(indicatif_layer.get_stderr_writer())
                .without_time(),
        )
        .with(indicatif_layer)
        .init();

    let command: Commands = cli.command.wrap_err("no subcommand provided").unwrap();
    match command {
        Commands::Compress {
            input_file,
            output_file,
            level,
        } => {
            let output_file = output_file.unwrap_or_else(|| add_extension(&input_file, ".zst"));
            compress(input_file, output_file, level)?;
        }
        Commands::Decompress {
            input_file,
            output_file,
        } => {
            let output_file = output_file.unwrap_or(
                input_file
                    .file_stem()
                    .expect("input has a file name")
                    .into(),
            );
            decompress(input_file, output_file)?;
        }
    }
    Ok(())
}

fn compress(input: PathBuf, output: PathBuf, level: u8) -> color_eyre::Result<()> {
    info!("compressing {input:?} to {output:?}");
    let compression_level: structured_zstd::encoding::CompressionLevel = match level {
        0 => CompressionLevel::Uncompressed,
        1 => CompressionLevel::Fastest,
        2 => CompressionLevel::Default,
        3 => CompressionLevel::Better,
        4 => CompressionLevel::Best,
        _ => return Err(eyre!("unsupported compression level: {level}")),
    };
    ensure_distinct_paths(&input, &output)?;
    ensure_regular_output_destination(&output)?;

    let source_file = File::open(&input).wrap_err("failed to open input file")?;
    let source_size: usize = source_file
        .metadata()?
        .len()
        .try_into()
        .wrap_err("input file too large for this platform")?;
    let buffered_source = BufReader::new(source_file);
    let mut encoder_input = ProgressMonitor::new(buffered_source, source_size);

    let (temporary_output_path, temporary_output) = create_temporary_output_file(&output)?;

    let compression_result: color_eyre::Result<File> = (|| {
        let mut encoder =
            structured_zstd::encoding::StreamingEncoder::new(temporary_output, compression_level);
        std::io::copy(&mut encoder_input, &mut encoder).wrap_err("streaming compression failed")?;
        encoder.finish().wrap_err("failed to finalize zstd frame")
    })();

    let temporary_output = match compression_result {
        Ok(file) => file,
        Err(err) => {
            let _ = fs::remove_file(&temporary_output_path);
            return Err(err);
        }
    };

    let compressed_size = match temporary_output.metadata() {
        Ok(metadata) => metadata.len(),
        Err(err) => {
            drop(temporary_output);
            let _ = fs::remove_file(&temporary_output_path);
            return Err(err).wrap_err("failed to get compressed file size");
        }
    };
    drop(temporary_output);
    replace_output_file(&temporary_output_path, &output)?;

    let compression_ratio = if source_size == 0 {
        0.0
    } else {
        compressed_size as f64 / source_size as f64 * 100.0
    };
    info!(
        "{} ——> {} ({compression_ratio:.2}%)",
        fmt_size(source_size as f64),
        fmt_size(compressed_size as f64)
    );
    Ok(())
}

fn ensure_distinct_paths(input: &Path, output: &Path) -> color_eyre::Result<()> {
    let canonical_input = match fs::canonicalize(input) {
        Ok(path) => path,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(err).wrap_err("failed to open input file");
        }
        Err(err) => {
            return Err(err).wrap_err("failed to canonicalize input file");
        }
    };
    if output.exists() {
        let canonical_output =
            fs::canonicalize(output).wrap_err("failed to canonicalize existing output file")?;
        if canonical_input == canonical_output || paths_point_to_same_file(input, output)? {
            return Err(eyre!(
                "input and output paths refer to the same file: {input:?} -> {output:?}"
            ));
        }
    }
    Ok(())
}

fn paths_point_to_same_file(input: &Path, output: &Path) -> color_eyre::Result<bool> {
    let input_metadata = fs::metadata(input).wrap_err("failed to inspect input file metadata")?;
    let output_metadata =
        fs::metadata(output).wrap_err("failed to inspect existing output file metadata")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(input_metadata.dev() == output_metadata.dev()
            && input_metadata.ino() == output_metadata.ino())
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Ok(
            input_metadata.volume_serial_number() == output_metadata.volume_serial_number()
                && input_metadata.file_index() == output_metadata.file_index(),
        )
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = input_metadata;
        let _ = output_metadata;
        Ok(false)
    }
}

#[cfg(windows)]
fn create_temporary_output_path(output: &Path) -> color_eyre::Result<PathBuf> {
    let (path, file) = create_temporary_output_file(output)?;
    drop(file);
    fs::remove_file(&path).wrap_err("failed to reserve temporary output path")?;
    Ok(path)
}

fn create_temporary_output_file(output: &Path) -> color_eyre::Result<(PathBuf, File)> {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let file_name = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output.zst");
    for attempt in 0..u16::MAX {
        let candidate = parent.join(format!(
            ".{file_name}.tmp.{}.{}",
            std::process::id(),
            attempt
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).wrap_err("failed to create temporary output file");
            }
        }
    }
    Err(eyre!("failed to allocate unique temporary output file"))
}

fn replace_output_file(temporary_output_path: &Path, output: &Path) -> color_eyre::Result<()> {
    let output_kind = match output_destination_kind(output).inspect_err(|_err| {
        let _ = fs::remove_file(temporary_output_path);
    })? {
        Some(kind) => kind,
        None => {
            return match fs::rename(temporary_output_path, output) {
                Ok(()) => Ok(()),
                Err(err) => {
                    let _ = fs::remove_file(temporary_output_path);
                    Err(err).wrap_err("failed to move temporary output file into final location")
                }
            };
        }
    };
    if !output_kind.is_file() {
        let _ = fs::remove_file(temporary_output_path);
        return Err(eyre!(
            "output path exists and is not a regular file: {output:?}"
        ));
    }
    let original_permissions = fs::metadata(output)
        .wrap_err("failed to read existing output file metadata")
        .inspect_err(|_err| {
            let _ = fs::remove_file(temporary_output_path);
        })?
        .permissions();
    if let Err(err) = fs::set_permissions(temporary_output_path, original_permissions.clone()) {
        let _ = fs::remove_file(temporary_output_path);
        return Err(err).wrap_err("failed to apply existing output permissions to temporary file");
    }

    #[cfg(not(windows))]
    {
        match fs::rename(temporary_output_path, output) {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = fs::remove_file(temporary_output_path);
                Err(err).wrap_err("failed to move temporary output file into final location")
            }
        }
    }

    #[cfg(windows)]
    {
        let backup_output_path = match create_temporary_output_path(output) {
            Ok(path) => path,
            Err(err) => {
                let _ = fs::remove_file(temporary_output_path);
                return Err(err).wrap_err("failed to allocate backup output path");
            }
        };
        if let Err(err) = fs::rename(output, &backup_output_path) {
            let _ = fs::remove_file(temporary_output_path);
            return Err(err).wrap_err("failed to move existing output file into backup location");
        }

        if let Err(err) = fs::rename(temporary_output_path, output) {
            let restore_result = fs::rename(&backup_output_path, output);
            let _ = fs::remove_file(temporary_output_path);
            if let Err(restore_err) = restore_result {
                return Err(err).wrap_err(format!(
                "failed to move temporary output file into final location; also failed to restore backup from {backup_output_path:?}: {restore_err}"
            ));
            }
            return Err(err).wrap_err("failed to move temporary output file into final location");
        }

        let _ = fs::remove_file(&backup_output_path);
        Ok(())
    }
}

fn output_destination_kind(output: &Path) -> color_eyre::Result<Option<std::fs::FileType>> {
    match fs::symlink_metadata(output) {
        Ok(metadata) => Ok(Some(metadata.file_type())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).wrap_err("failed to inspect existing output path"),
    }
}

fn ensure_regular_output_destination(output: &Path) -> color_eyre::Result<()> {
    if output_destination_kind(output)?.is_some_and(|kind| !kind.is_file()) {
        return Err(eyre!(
            "output path exists and is not a regular file: {output:?}"
        ));
    }
    Ok(())
}

fn decompress(input: PathBuf, output: PathBuf) -> color_eyre::Result<()> {
    info!("extracting {input:?} to {output:?}");
    let source_file = File::open(input).wrap_err("failed to open input file")?;
    let source_size = source_file.metadata()?.len() as usize;
    let buffered_source = BufReader::new(source_file);
    let decoder_input = ProgressMonitor::new(buffered_source, source_size);
    let mut output: File =
        File::create(output).wrap_err("failed to open output file for writing")?;

    let mut decoder = structured_zstd::decoding::StreamingDecoder::new(decoder_input)?;

    std::io::copy(&mut decoder, &mut output)?;

    info!(
        "inflated {} ——> {}",
        fmt_size(source_size as f64),
        fmt_size(output.metadata()?.len() as f64),
    );
    Ok(())
}

/// A temporary utility function that appends a file extension
/// to the provided path buf.
///
/// Pending removal when our MSRV reaches 1.91 so we can use
///
/// <https://doc.rust-lang.org/std/path/struct.PathBuf.html#method.add_extension>
fn add_extension<P: AsRef<Path>>(path: &Path, extension: P) -> PathBuf {
    let mut output = path.to_path_buf().into_os_string();
    output.push(extension.as_ref().as_os_str());

    output.into()
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use clap::Parser;

    use super::{Cli, compress, replace_output_file};
    use std::path::PathBuf;

    use crate::add_extension;

    #[test]
    fn extension_added() {
        let filename = PathBuf::from("README.md");
        assert_eq!(
            add_extension(&filename, ".zst"),
            PathBuf::from("README.md.zst")
        );
    }

    #[test]
    fn cli_rejects_unsupported_compression_level_at_parse_time() {
        let parse = Cli::try_parse_from(["structured-zstd", "compress", "in.bin", "--level", "5"]);
        assert!(parse.is_err());
    }

    #[test]
    fn compress_rejects_same_input_and_output_paths() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let input = std::env::temp_dir().join(format!("structured-zstd-cli-alias-{unique}.txt"));
        fs::write(&input, b"streaming-cli-alias-check").unwrap();

        let err = compress(input.clone(), input.clone(), 2).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("input and output"),
            "unexpected error: {message}"
        );

        let _ = fs::remove_file(input);
    }

    #[cfg(unix)]
    #[test]
    fn compress_rejects_hardlinked_output_paths() {
        let dir = unique_test_dir("structured-zstd-cli-hardlink-alias");
        let input = dir.join("input.txt");
        let output = dir.join("output.zst");
        fs::write(&input, b"streaming-cli-hardlink-check").unwrap();
        fs::hard_link(&input, &output).unwrap();

        let err = compress(input.clone(), output.clone(), 2).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("input and output"),
            "unexpected error: {message}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn compress_reports_open_error_for_missing_input() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let missing_input =
            std::env::temp_dir().join(format!("structured-zstd-cli-missing-input-{unique}.txt"));
        let output =
            std::env::temp_dir().join(format!("structured-zstd-cli-missing-output-{unique}.zst"));

        let err = compress(missing_input, output.clone(), 2).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("failed to open input file"),
            "unexpected error: {message}"
        );

        let _ = fs::remove_file(output);
    }

    #[test]
    fn compress_rejects_non_regular_output_before_creating_temp_file() {
        let dir = unique_test_dir("structured-zstd-cli-preflight-output");
        let input = dir.join("input.txt");
        write_file(&input, b"streaming-cli-preflight");
        let output = dir.join("existing-dir");
        fs::create_dir(&output).unwrap();

        let err = compress(input, output.clone(), 2).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("not a regular file"),
            "unexpected error: {message}"
        );

        let tmp_count = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(tmp_count, 0, "temporary output should not be created");

        let _ = fs::remove_dir_all(dir);
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{prefix}-{unique}"));
        fs::create_dir(&dir).unwrap();
        dir
    }

    fn write_file(path: &Path, content: &[u8]) {
        fs::write(path, content).unwrap();
    }

    #[test]
    fn replace_output_file_rejects_non_regular_destination() {
        let dir = unique_test_dir("structured-zstd-cli-non-regular");
        let temporary_output = dir.join("result.tmp");
        write_file(&temporary_output, b"compressed");
        let destination = dir.join("existing-dir");
        fs::create_dir(&destination).unwrap();

        let err = replace_output_file(&temporary_output, &destination).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("not a regular file"),
            "unexpected error: {message}"
        );
        assert!(destination.is_dir());
        assert!(
            !temporary_output.exists(),
            "temporary file should be cleaned up when destination is invalid"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn replace_output_file_preserves_existing_permissions() {
        let dir = unique_test_dir("structured-zstd-cli-preserve-permissions");
        let destination = dir.join("result.zst");
        write_file(&destination, b"old-data");
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o640)).unwrap();

        let temporary_output = dir.join("result.tmp");
        write_file(&temporary_output, b"new-data");
        fs::set_permissions(&temporary_output, fs::Permissions::from_mode(0o600)).unwrap();

        replace_output_file(&temporary_output, &destination).unwrap();

        let mode = fs::metadata(&destination).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        assert_eq!(fs::read(&destination).unwrap(), b"new-data");
        assert!(!temporary_output.exists());

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn replace_output_file_rejects_broken_symlink_destination() {
        let dir = unique_test_dir("structured-zstd-cli-broken-symlink");
        let temporary_output = dir.join("result.tmp");
        write_file(&temporary_output, b"compressed");
        let destination = dir.join("result.zst");
        let missing_target = dir.join("missing-target.zst");
        symlink(&missing_target, &destination).unwrap();

        let err = replace_output_file(&temporary_output, &destination).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("not a regular file"),
            "unexpected error: {message}"
        );
        assert!(
            !temporary_output.exists(),
            "temporary file should be cleaned up when destination is invalid"
        );
        assert!(
            fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink(),
            "destination symlink should remain untouched"
        );

        let _ = fs::remove_dir_all(dir);
    }
}
