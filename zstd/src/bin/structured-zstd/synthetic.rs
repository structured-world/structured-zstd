//! The data `-b` measures when no input is named: lorem ipsum text, or with
//! `-P#` a generated mix of literals and matches of the given compressibility.
//! Both generators are seeded the same way as the reference command's
//! (`programs/lorem.c` and `programs/datagen.c`), so the two benchmarks measure
//! the same bytes.

/// How many bytes the reference command generates when `-B` sets no size
/// (`benchzstd.c`, `BMK_syntheticTest`).
pub(crate) const DEFAULT_SIZE: usize = 10_000_000;

/// The rotate-multiply step both generators draw from.
fn next_state(state: u32) -> u32 {
    (state.wrapping_mul(2_654_435_761) ^ 2_246_822_519).rotate_left(13)
}

/// Generated data where `match_percent` percent of the decisions copy an
/// earlier run of bytes and the rest emit literals from a skewed alphabet
/// (`RDG_genBuffer` with seed 0). At 100 and above the content is sparse: long
/// runs of zero bytes, each ended by one literal.
pub(crate) fn compressible(size: usize, match_percent: u32) -> Vec<u8> {
    let match_proba = f64::from(match_percent) / 100.0;
    let literal_proba = match_proba / 4.5;
    let literals = literal_table((literal_proba * 256.0 + 0.001) as u32);
    let mut seed = 0u32;
    let mut out = vec![0u8; size];
    fill_block(&mut out, match_proba, &literals, &mut seed);
    out
}

const LITERAL_TABLE_LOG: u32 = 13;
const LITERAL_TABLE_SIZE: usize = 1 << LITERAL_TABLE_LOG;

/// Draw a 27-bit value (`RDG_rand`).
fn draw(seed: &mut u32) -> u32 {
    *seed = next_state(*seed);
    *seed >> 5
}

/// The alphabet literals are drawn from: each next character takes a smaller
/// share of the table, so a higher `skew` (in 1/256ths) concentrates the
/// literals on fewer characters. Zero spreads them over every byte value
/// (`RDG_fillLiteralDistrib`).
fn literal_table(skew: u32) -> [u8; LITERAL_TABLE_SIZE] {
    let (first, last, mut character) = if skew == 0 {
        (0u8, 255u8, 0u8)
    } else {
        (b'(', b'}', b'0')
    };
    let mut table = [0u8; LITERAL_TABLE_SIZE];
    let mut at = 0usize;
    while at < LITERAL_TABLE_SIZE {
        let remaining = (LITERAL_TABLE_SIZE - at) as u32;
        let weight = (remaining.wrapping_mul(skew) >> 8) as usize + 1;
        let end = (at + weight).min(LITERAL_TABLE_SIZE);
        table[at..end].fill(character);
        at = end;
        character = character.wrapping_add(1);
        if character > last {
            character = first;
        }
    }
    table
}

fn literal(seed: &mut u32, table: &[u8; LITERAL_TABLE_SIZE]) -> u8 {
    table[(draw(seed) as usize) & (LITERAL_TABLE_SIZE - 1)]
}

/// A run length: mostly below 16, one draw in eight between 15 and 525.
fn run_length(seed: &mut u32) -> usize {
    if draw(seed) & 7 != 0 {
        (draw(seed) & 0xF) as usize
    } else {
        (draw(seed) & 0x1FF) as usize + 0xF
    }
}

/// `RDG_genBlock` from position 0.
fn fill_block(out: &mut [u8], match_proba: f64, table: &[u8; LITERAL_TABLE_SIZE], seed: &mut u32) {
    let size = out.len();
    let mut pos = 0usize;
    if match_proba >= 1.0 {
        loop {
            let class = draw(seed) & 3;
            let mut run = 1usize << (16 + class * 2);
            run += (draw(seed) as usize) & (run - 1);
            if size < pos + run {
                // The buffer starts zeroed, so the tail already is.
                return;
            }
            pos += run;
            out[pos - 1] = literal(seed, table);
        }
    }
    if size == 0 {
        return;
    }
    let match_threshold = (32768.0 * match_proba) as u32;
    out[0] = literal(seed, table);
    pos = 1;
    let mut previous_offset = 1usize;
    while pos < size {
        if draw(seed) & 0x7FFF < match_threshold {
            let end = (pos + run_length(seed) + 4).min(size);
            let repeat = draw(seed) & 15 == 2;
            let random_offset = (draw(seed) & 0x7FFF) as usize + 1;
            let offset = if repeat {
                previous_offset
            } else {
                random_offset.min(pos)
            };
            // Byte by byte: the source may overlap what is being written.
            while pos < end {
                out[pos] = out[pos - offset];
                pos += 1;
            }
            previous_offset = offset;
        } else {
            let end = (pos + run_length(seed)).min(size);
            while pos < end {
                out[pos] = literal(seed, table);
                pos += 1;
            }
        }
    }
}

const WORDS: [&str; 255] = [
    "lorem",
    "ipsum",
    "dolor",
    "sit",
    "amet",
    "consectetur",
    "adipiscing",
    "elit",
    "sed",
    "do",
    "eiusmod",
    "tempor",
    "incididunt",
    "ut",
    "labore",
    "et",
    "dolore",
    "magna",
    "aliqua",
    "dis",
    "lectus",
    "vestibulum",
    "mattis",
    "ullamcorper",
    "velit",
    "commodo",
    "a",
    "lacus",
    "arcu",
    "magnis",
    "parturient",
    "montes",
    "nascetur",
    "ridiculus",
    "mus",
    "mauris",
    "nulla",
    "malesuada",
    "pellentesque",
    "eget",
    "gravida",
    "in",
    "dictum",
    "non",
    "erat",
    "nam",
    "voluptat",
    "maecenas",
    "blandit",
    "aliquam",
    "etiam",
    "enim",
    "lobortis",
    "scelerisque",
    "fermentum",
    "dui",
    "faucibus",
    "ornare",
    "at",
    "elementum",
    "eu",
    "facilisis",
    "odio",
    "morbi",
    "quis",
    "eros",
    "donec",
    "ac",
    "orci",
    "purus",
    "turpis",
    "cursus",
    "leo",
    "vel",
    "porta",
    "consequat",
    "interdum",
    "varius",
    "vulputate",
    "aliquet",
    "pharetra",
    "nunc",
    "auctor",
    "urna",
    "id",
    "metus",
    "viverra",
    "nibh",
    "cras",
    "mi",
    "unde",
    "omnis",
    "iste",
    "natus",
    "error",
    "perspiciatis",
    "voluptatem",
    "accusantium",
    "doloremque",
    "laudantium",
    "totam",
    "rem",
    "aperiam",
    "eaque",
    "ipsa",
    "quae",
    "ab",
    "illo",
    "inventore",
    "veritatis",
    "quasi",
    "architecto",
    "beatae",
    "vitae",
    "dicta",
    "sunt",
    "explicabo",
    "nemo",
    "ipsam",
    "quia",
    "voluptas",
    "aspernatur",
    "aut",
    "odit",
    "fugit",
    "consequuntur",
    "magni",
    "dolores",
    "eos",
    "qui",
    "ratione",
    "sequi",
    "nesciunt",
    "neque",
    "porro",
    "quisquam",
    "est",
    "dolorem",
    "adipisci",
    "numquam",
    "eius",
    "modi",
    "tempora",
    "incidunt",
    "magnam",
    "quaerat",
    "ad",
    "minima",
    "veniam",
    "nostrum",
    "ullam",
    "corporis",
    "suscipit",
    "laboriosam",
    "nisi",
    "aliquid",
    "ex",
    "ea",
    "commodi",
    "consequatur",
    "autem",
    "eum",
    "iure",
    "voluptate",
    "esse",
    "quam",
    "nihil",
    "molestiae",
    "illum",
    "fugiat",
    "quo",
    "pariatur",
    "vero",
    "accusamus",
    "iusto",
    "dignissimos",
    "ducimus",
    "blanditiis",
    "praesentium",
    "voluptatum",
    "deleniti",
    "atque",
    "corrupti",
    "quos",
    "quas",
    "molestias",
    "excepturi",
    "sint",
    "occaecati",
    "cupiditate",
    "provident",
    "similique",
    "culpa",
    "officia",
    "deserunt",
    "mollitia",
    "animi",
    "laborum",
    "dolorum",
    "fuga",
    "harum",
    "quidem",
    "rerum",
    "facilis",
    "expedita",
    "distinctio",
    "libero",
    "tempore",
    "cum",
    "soluta",
    "nobis",
    "eligendi",
    "optio",
    "cumque",
    "impedit",
    "minus",
    "quod",
    "maxime",
    "placeat",
    "facere",
    "possimus",
    "assumenda",
    "repellendus",
    "temporibus",
    "quibusdam",
    "officiis",
    "debitis",
    "saepe",
    "eveniet",
    "voluptates",
    "repudiandae",
    "recusandae",
    "itaque",
    "earum",
    "hic",
    "tenetur",
    "sapiente",
    "delectus",
    "reiciendis",
    "cillum",
    "maiores",
    "alias",
    "perferendis",
    "doloribus",
    "asperiores",
    "repellat",
    "minim",
    "nostrud",
    "exercitation",
    "ullamco",
    "laboris",
    "aliquip",
    "duis",
    "aute",
    "irure",
];

/// How often a word is drawn, by its length: short words more often, every
/// word of five letters or more equally rarely.
const WEIGHT_BY_LENGTH: [usize; 6] = [0, 8, 6, 4, 3, 2];

/// Lorem ipsum text of exactly `size` bytes (`LOREM_genBuffer` with seed 0):
/// the customary first sentence, then random sentences of the word list in
/// paragraphs, the last one cut and padded to the size.
pub(crate) fn lorem(size: usize) -> Vec<u8> {
    let mut words = Vec::with_capacity(650);
    for (id, word) in WORDS.iter().enumerate() {
        let weight = WEIGHT_BY_LENGTH[word.len().min(WEIGHT_BY_LENGTH.len() - 1)];
        words.extend(std::iter::repeat_n(id, weight));
    }
    let mut text = Lorem {
        out: Vec::with_capacity(size),
        size,
        state: 0,
        words,
    };
    text.first_sentence();
    while text.out.len() < size {
        let sentences = text.about(7);
        text.paragraph(sentences);
    }
    text.out
}

struct Lorem {
    out: Vec<u8>,
    size: usize,
    state: u32,
    /// Word indices, each repeated by its weight, so one uniform draw picks a
    /// word with that weight.
    words: Vec<usize>,
}

impl Lorem {
    /// A uniform draw below `range` (`LOREM_rand`).
    fn draw(&mut self, range: u32) -> u32 {
        self.state = next_state(self.state);
        ((u64::from(self.state) * u64::from(range)) >> 32) as u32
    }

    /// Roughly `target`, between 1 and `2 * target - 1`.
    fn about(&mut self, target: u32) -> usize {
        (self.draw(target) + self.draw(target) + 1) as usize
    }

    /// Append a word and its separator, or, when they no longer fit, close
    /// the text: a full stop, spaces, and a newline in the last byte.
    fn word(&mut self, word: &str, separator: &str, capital: bool) {
        if self.out.len() + word.len() + separator.len() > self.size {
            let left = self.size - self.out.len();
            if left > 0 {
                self.out.push(b'.');
                self.out.resize(self.size, b' ');
                if left > 1 {
                    self.out[self.size - 1] = b'\n';
                }
            }
            return;
        }
        let start = self.out.len();
        self.out.extend_from_slice(word.as_bytes());
        if capital {
            self.out[start] = self.out[start].to_ascii_uppercase();
        }
        self.out.extend_from_slice(separator.as_bytes());
    }

    fn sentence(&mut self, length: usize) {
        let comma = self.about(9);
        let second_comma = comma + self.about(7);
        let end = if self.draw(11) == 7 { "? " } else { ". " };
        for at in 0..length {
            let pick = self.draw(self.words.len() as u32) as usize;
            let word = WORDS[self.words[pick]];
            let separator = if at == length - 1 {
                end
            } else if at == comma || at == second_comma {
                ", "
            } else {
                " "
            };
            self.word(word, separator, at == 0);
        }
    }

    fn paragraph(&mut self, sentences: usize) {
        for _ in 0..sentences {
            let length = self.about(11);
            self.sentence(length);
        }
        for _ in 0..2 {
            if self.out.len() < self.size {
                self.out.push(b'\n');
            }
        }
    }

    fn first_sentence(&mut self) {
        for (at, word) in WORDS[..18].iter().enumerate() {
            let separator = if at == 4 || at == 7 { ", " } else { " " };
            self.word(word, separator, at == 0);
        }
        self.word(WORDS[18], ". ", false);
    }
}

#[cfg(test)]
mod tests;
