//! Парсер bulk-импорта: одна запись на строку, `позывной [@username]`.
//! Разделители внутри строки: пробелы, табы, `;`, `,`. Чистые функции (design.md D7).

const MAX_CALLSIGN_CHARS: usize = 64;
const MAX_USERNAME_CHARS: usize = 32;

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedEntry {
    pub callsign: String,
    /// Уже нормализован: без `@`, lowercase (username в Telegram регистронезависимы).
    pub username: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct LineError {
    pub line_no: usize,
    pub reason: String,
}

pub fn parse_roster_lines(text: &str) -> Vec<Result<ParsedEntry, LineError>> {
    text.lines()
        .enumerate()
        .filter_map(|(i, raw)| {
            parse_line(raw).map(|r| r.map_err(|reason| LineError { line_no: i + 1, reason }))
        })
        .collect()
}

/// None — пустая строка (игнорируется).
fn parse_line(raw: &str) -> Option<Result<ParsedEntry, String>> {
    let cleaned = raw.replace([';', ',', '\t'], " ");
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    let first = *tokens.first()?;

    if first.starts_with('@') {
        return Some(Err("нет позывного (строка начинается с @username)".into()));
    }
    if first.chars().count() > MAX_CALLSIGN_CHARS {
        return Some(Err(format!("позывной длиннее {MAX_CALLSIGN_CHARS} символов")));
    }

    let username = match tokens.as_slice() {
        [_] => None,
        [_, u] => {
            let u = u.trim_start_matches('@');
            if !is_valid_username(u) {
                return Some(Err(format!("некорректный username: «{u}»")));
            }
            Some(u.to_lowercase())
        }
        // Больше двух токенов — скорее всего многословный позывной или мусор:
        // честная диагностика вместо угадывания.
        _ => {
            return Some(Err(
                "лишние данные в строке (ожидается: позывной [@username])".into()
            ))
        }
    };

    Some(Ok(ParsedEntry { callsign: first.to_string(), username }))
}

fn is_valid_username(u: &str) -> bool {
    !u.is_empty()
        && u.chars().count() <= MAX_USERNAME_CHARS
        && u.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(callsign: &str, username: Option<&str>) -> Result<ParsedEntry, LineError> {
        Ok(ParsedEntry { callsign: callsign.to_string(), username: username.map(String::from) })
    }

    #[test]
    fn single_callsign() {
        assert_eq!(parse_roster_lines("Ястреб"), vec![ok("Ястреб", None)]);
    }

    #[test]
    fn callsign_with_username() {
        assert_eq!(
            parse_roster_lines("Сокол @ivan_petrov"),
            vec![ok("Сокол", Some("ivan_petrov"))]
        );
    }

    #[test]
    fn username_without_at_and_uppercase_normalized() {
        assert_eq!(parse_roster_lines("Сокол Ivan_Petrov"), vec![ok("Сокол", Some("ivan_petrov"))]);
    }

    #[test]
    fn separators_semicolon_comma_tab() {
        let text = "Сокол;@ivan\nБеркут,@petr\nГриф\t@oleg";
        assert_eq!(
            parse_roster_lines(text),
            vec![ok("Сокол", Some("ivan")), ok("Беркут", Some("petr")), ok("Гриф", Some("oleg"))]
        );
    }

    #[test]
    fn empty_lines_ignored() {
        let text = "\nСокол\n\n   \nБеркут\n";
        assert_eq!(parse_roster_lines(text), vec![ok("Сокол", None), ok("Беркут", None)]);
    }

    #[test]
    fn line_with_only_username_is_error_with_line_no() {
        let text = "Сокол\n@ivan_petrov\nБеркут";
        let parsed = parse_roster_lines(text);
        assert_eq!(parsed.len(), 3);
        assert!(parsed[0].is_ok() && parsed[2].is_ok());
        let err = parsed[1].as_ref().unwrap_err();
        assert_eq!(err.line_no, 2);
        assert!(err.reason.contains("нет позывного"));
    }

    #[test]
    fn invalid_username_chars_is_error() {
        let parsed = parse_roster_lines("Сокол @иван");
        assert!(parsed[0].as_ref().unwrap_err().reason.contains("некорректный username"));
    }

    #[test]
    fn extra_tokens_is_error() {
        let parsed = parse_roster_lines("Сокол Иван @ivan");
        assert!(parsed[0].as_ref().unwrap_err().reason.contains("лишние данные"));
    }

    #[test]
    fn empty_input() {
        assert!(parse_roster_lines("").is_empty());
    }
}
