use std::collections::BTreeSet;
use std::sync::OnceLock;

use jieba_rs::Jieba;

static BASE_JIEBA: OnceLock<Jieba> = OnceLock::new();
static SHARED_TOKENIZER: OnceLock<Tokenizer> = OnceLock::new();

#[inline]
fn base_jieba() -> &'static Jieba {
    BASE_JIEBA.get_or_init(Jieba::new)
}

#[derive(Default)]
pub struct Tokenizer {
    custom: OnceLock<Box<Jieba>>,
    words: BTreeSet<String>,
}

impl Tokenizer {
    /// Process-wide clean tokenizer backed by `BASE_JIEBA` with zero allocation or cloning.
    #[inline]
    pub fn shared() -> &'static Self {
        SHARED_TOKENIZER.get_or_init(Self::default)
    }

    #[inline]
    pub fn has_custom_words(&self) -> bool {
        !self.words.is_empty()
    }

    pub fn add_entity_word(&mut self, word: &str) {
        if !word.is_ascii() && !word.trim().is_empty() && self.words.insert(word.into()) {
            if let Some(jieba) = self.custom.get_mut() {
                jieba.add_word(word, Some(1_000_000), None);
            }
        }
    }

    pub fn tokenize(&self, text: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut start = 0;
        let mut ascii = None;
        for (index, character) in text.char_indices() {
            let is_identifier = character.is_ascii_alphanumeric() || character == '_';
            if ascii.is_some_and(|previous| previous != is_identifier) {
                self.segment(&text[start..index], ascii == Some(true), &mut tokens);
                start = index;
            }
            ascii = Some(is_identifier);
        }
        self.segment(&text[start..], ascii == Some(true), &mut tokens);
        tokens
    }

    pub fn segment_text(&self, text: &str) -> String {
        self.tokenize(text).join(" ")
    }

    fn segment(&self, text: &str, ascii: bool, tokens: &mut Vec<String>) {
        if !text.chars().any(char::is_alphanumeric) {
            return;
        }
        if !ascii {
            let jieba: &Jieba = if self.words.is_empty() {
                base_jieba()
            } else {
                self.custom
                    .get_or_init(|| {
                        let mut jieba = Box::new(base_jieba().clone());
                        for word in &self.words {
                            jieba.add_word(word, Some(1_000_000), None);
                        }
                        jieba
                    })
                    .as_ref()
            };
            tokens.extend(
                jieba
                    .cut_for_search(text, true)
                    .into_iter()
                    .filter(|word| word.chars().any(char::is_alphanumeric))
                    .map(str::to_lowercase),
            );
            return;
        }
        if !text.bytes().any(|b| b.is_ascii_alphanumeric()) {
            return;
        }
        let whole = text.to_ascii_lowercase();
        tokens.push(whole.clone());
        for part in text.split('_').filter(|p| !p.is_empty()) {
            let bytes = part.as_bytes();
            let mut start = 0;
            for index in 1..bytes.len() {
                let boundary = bytes[index].is_ascii_uppercase()
                    && (bytes[index - 1].is_ascii_lowercase()
                        || bytes[index - 1].is_ascii_digit()
                        || (bytes[index - 1].is_ascii_uppercase()
                            && bytes.get(index + 1).is_some_and(u8::is_ascii_lowercase)));
                if boundary {
                    let token = part[start..index].to_ascii_lowercase();
                    if token != whole {
                        tokens.push(token);
                    }
                    start = index;
                }
            }
            let token = part[start..].to_ascii_lowercase();
            if token != whole {
                tokens.push(token);
            }
        }
    }
}

#[cfg(test)]
#[path = "tokenizer_tests.rs"]
mod tests;
