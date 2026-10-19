//! Prompts the merchant typed, newest last, for recalling with the arrow keys.

/// Older prompts beyond this are forgotten.
pub const LIMIT: usize = 200;

/// Adds `text` as the newest prompt, moving an earlier copy instead of
/// keeping both.
pub fn remember(entries: &mut Vec<String>, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    entries.retain(|e| e != text);
    entries.push(text.to_string());
    if entries.len() > LIMIT {
        entries.drain(..entries.len() - LIMIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeats_move_to_the_end_and_the_oldest_drop_past_the_limit() {
        let mut e = Vec::new();
        for t in ["a", " b ", "", "a"] {
            remember(&mut e, t);
        }
        assert_eq!(e, ["b", "a"]);
        for i in 0..LIMIT {
            remember(&mut e, &i.to_string());
        }
        assert_eq!(e.len(), LIMIT);
        assert_eq!(e[0], "0");
        assert_eq!(e[LIMIT - 1], (LIMIT - 1).to_string());
    }
}
