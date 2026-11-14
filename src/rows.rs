use std::borrow::Cow;

use rls2fga::types::{ColumnKind, RowCell, RowList, RowValues};
use serde_json::Value;

pub(crate) struct Row<'a>(pub &'a Value);

/// Render a typed scalar for the translation's row evaluator.
fn scalar(value: &Value, kind: ColumnKind) -> RowCell<'_> {
    match (value, kind) {
        (Value::Null, _) => RowCell::Null,
        (Value::String(text), ColumnKind::Text | ColumnKind::Bpchar) => {
            RowCell::Text(Cow::Borrowed(text))
        }
        (Value::String(text), ColumnKind::Uuid) => RowCell::Uuid(Cow::Borrowed(text)),
        (Value::String(text), ColumnKind::Date) => RowCell::Date(Cow::Borrowed(text)),
        (Value::String(text), ColumnKind::Time) => RowCell::Time(Cow::Borrowed(text)),
        (Value::String(text), ColumnKind::Timestamp) => RowCell::Timestamp(Cow::Borrowed(text)),
        (Value::String(text), ColumnKind::TimestampTz) => RowCell::TimestampTz(Cow::Borrowed(text)),
        (Value::Number(number), ColumnKind::Integer) => {
            RowCell::Integer(Cow::Owned(number.to_string()))
        }
        (Value::Number(number), ColumnKind::Decimal) => {
            RowCell::Decimal(Cow::Owned(number.to_string()))
        }
        (Value::Bool(flag), ColumnKind::Bool) => RowCell::Bool(*flag),
        _ => RowCell::Undecodable,
    }
}

impl RowValues for Row<'_> {
    fn cell(&self, column: &str, kind: ColumnKind) -> RowCell<'_> {
        self.0
            .get(column)
            .map_or(RowCell::Absent, |value| scalar(value, kind))
    }

    fn list(&self, column: &str, kind: ColumnKind) -> RowList<'_> {
        match self.0.get(column) {
            Some(Value::Null) => RowList::Null,
            Some(Value::Array(values)) => {
                RowList::Values(values.iter().map(|value| scalar(value, kind)).collect())
            }
            None => RowList::Absent,
            Some(_) => RowList::Undecodable,
        }
    }

    fn json_text(&self, column: &str, path: &[String]) -> RowCell<'_> {
        let Some(mut value) = self.0.get(column) else {
            return RowCell::Absent;
        };
        for key in path {
            let Some(next) = value.get(key) else {
                return RowCell::Null;
            };
            value = next;
        }
        match value {
            Value::Null => RowCell::Null,
            Value::String(text) => RowCell::Text(Cow::Borrowed(text)),
            Value::Number(_) | Value::Bool(_) => RowCell::Text(Cow::Owned(value.to_string())),
            _ => RowCell::Undecodable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Typed scalar cells retain their kind and reject incompatible input.
    #[test]
    fn scalar_cells() {
        let text = json!("value");
        for (kind, expected) in [
            (ColumnKind::Text, RowCell::Text("value".into())),
            (ColumnKind::Bpchar, RowCell::Text("value".into())),
            (ColumnKind::Uuid, RowCell::Uuid("value".into())),
            (ColumnKind::Date, RowCell::Date("value".into())),
            (ColumnKind::Time, RowCell::Time("value".into())),
            (ColumnKind::Timestamp, RowCell::Timestamp("value".into())),
            (
                ColumnKind::TimestampTz,
                RowCell::TimestampTz("value".into()),
            ),
        ] {
            assert_eq!(scalar(&text, kind), expected);
        }
        assert_eq!(scalar(&Value::Null, ColumnKind::Text), RowCell::Null);
        assert_eq!(
            scalar(&json!(7), ColumnKind::Integer),
            RowCell::Integer("7".into())
        );
        assert_eq!(
            scalar(&json!(1.5), ColumnKind::Decimal),
            RowCell::Decimal("1.5".into())
        );
        assert_eq!(scalar(&json!(true), ColumnKind::Bool), RowCell::Bool(true));
        assert_eq!(scalar(&text, ColumnKind::Bool), RowCell::Undecodable);
    }

    /// Missing, null and malformed values remain distinct for lists and cells.
    #[test]
    fn cells_and_lists() {
        let value = json!({"null":null,"text":"value","list":["a",null,7]});
        let row = Row(&value);
        assert_eq!(row.cell("missing", ColumnKind::Text), RowCell::Absent);
        assert_eq!(row.cell("null", ColumnKind::Text), RowCell::Null);
        assert_eq!(
            row.cell("text", ColumnKind::Text),
            RowCell::Text("value".into())
        );
        assert_eq!(row.list("missing", ColumnKind::Text), RowList::Absent);
        assert_eq!(row.list("null", ColumnKind::Text), RowList::Null);
        assert_eq!(row.list("text", ColumnKind::Text), RowList::Undecodable);
        assert_eq!(
            row.list("list", ColumnKind::Text),
            RowList::Values(vec![
                RowCell::Text("a".into()),
                RowCell::Null,
                RowCell::Undecodable
            ])
        );
    }

    /// JSON text extraction distinguishes absent columns from absent object keys.
    #[test]
    fn json_text_paths() {
        let value =
            json!({"doc":{"nested":{"text":"a","number":7,"bool":true,"null":null,"array":[]}}});
        let row = Row(&value);
        assert_eq!(row.json_text("missing", &[]), RowCell::Absent);
        assert_eq!(row.json_text("doc", &["missing".into()]), RowCell::Null);
        assert_eq!(row.json_text("doc", &[]), RowCell::Undecodable);
        for (key, expected) in [
            ("text", RowCell::Text("a".into())),
            ("number", RowCell::Text("7".into())),
            ("bool", RowCell::Text("true".into())),
            ("null", RowCell::Null),
            ("array", RowCell::Undecodable),
        ] {
            assert_eq!(
                row.json_text("doc", &["nested".into(), key.into()]),
                expected
            );
        }
    }
}
