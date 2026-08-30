#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct StoredMetricKey {
    pub family: String,
    pub sample: String,
    pub labels: String,
}

impl StoredMetricKey {
    pub fn new(
        family: impl Into<String>,
        sample: impl Into<String>,
        labels: impl Into<String>,
    ) -> Self {
        Self {
            family: family.into(),
            sample: sample.into(),
            labels: labels.into(),
        }
    }
}

pub fn encode_labels<'a, I>(labels: I) -> String
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut labels: Vec<(&str, &str)> = labels.into_iter().collect();
    if labels.is_empty() {
        return String::new();
    }

    labels.sort_by(|a, b| a.0.cmp(b.0));

    let mut out = String::new();
    out.push('{');
    for (idx, (name, value)) in labels.into_iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(name);
        out.push_str("=\"");
        out.push_str(&escape_label_value(value));
        out.push('"');
    }
    out.push('}');
    out
}

pub fn escape_label_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod test {
    use super::encode_labels;

    #[test]
    fn encode_labels_sorts_and_escapes() {
        let labels = encode_labels([("b", "2"), ("a", "1\n\"x")]);
        assert_eq!(labels, r#"{a="1\n\"x",b="2"}"#);
    }
}
