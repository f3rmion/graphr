use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AnalyzerKind {
    Generic,
    Markdown,
    Tsv,
}

impl AnalyzerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Markdown => "markdown",
            Self::Tsv => "tsv",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct Analysis {
    pub kind: AnalyzerKind,
    pub output: String,
}

type Analyzer = fn(&str, Option<&str>, Option<&str>) -> Result<String, String>;

struct AnalyzerEntry {
    extensions: &'static [&'static str],
    kind: AnalyzerKind,
    run: Analyzer,
}

const ANALYZERS: &[AnalyzerEntry] = &[
    AnalyzerEntry {
        extensions: &["md", "markdown"],
        kind: AnalyzerKind::Markdown,
        run: analyze_markdown,
    },
    AnalyzerEntry {
        extensions: &["tsv"],
        kind: AnalyzerKind::Tsv,
        run: analyze_tsv,
    },
];

pub fn analyzer_kind(path: &str) -> AnalyzerKind {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    ANALYZERS
        .iter()
        .find(|entry| {
            entry
                .extensions
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
        .map_or(AnalyzerKind::Generic, |entry| entry.kind)
}

pub fn analyze(path: &str, old: Option<&str>, new: Option<&str>) -> Result<Analysis, String> {
    let kind = analyzer_kind(path);
    let output = ANALYZERS
        .iter()
        .find(|entry| entry.kind == kind)
        .map_or_else(|| Ok(String::new()), |entry| (entry.run)(path, old, new))?;
    Ok(Analysis { kind, output })
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Field {
    name: &'static str,
    value: String,
    quoted: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RecordKey {
    kind: &'static str,
    fields: Vec<Field>,
}

#[derive(Clone, Debug)]
struct Record {
    key: RecordKey,
    line: usize,
    end_line: usize,
    issue: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Location {
    line: usize,
    end_line: usize,
}

impl Record {
    fn new(kind: &'static str, fields: Vec<Field>, line: usize) -> Self {
        Self {
            key: RecordKey { kind, fields },
            line,
            end_line: line,
            issue: false,
        }
    }

    fn issue(marker: String, line: usize) -> Self {
        Self {
            key: RecordKey {
                kind: "issue",
                fields: vec![field("issue", "unclosed-fence"), quoted("marker", marker)],
            },
            line,
            end_line: line,
            issue: true,
        }
    }
}

fn field(name: &'static str, value: impl Into<String>) -> Field {
    Field {
        name,
        value: value.into(),
        quoted: false,
    }
}

fn quoted(name: &'static str, value: impl Into<String>) -> Field {
    Field {
        name,
        value: value.into(),
        quoted: true,
    }
}

fn analyze_markdown(path: &str, old: Option<&str>, new: Option<&str>) -> Result<String, String> {
    let old_records = old.map(markdown_records).unwrap_or_default();
    let new_records = new.map(markdown_records).unwrap_or_default();
    let mut output = diff_records(path, &old_records, &new_records);
    output.extend(
        new_records
            .into_iter()
            .filter(|record| record.issue)
            .map(|record| output_line(path, None, &record)),
    );
    output.sort();
    Ok(output
        .into_iter()
        .map(|line| line.text)
        .collect::<Vec<_>>()
        .join("\n"))
}

fn markdown_records(text: &str) -> Vec<Record> {
    let mut records = Vec::new();
    let mut fence: Option<(char, usize, String, usize, String)> = None;

    for (index, raw) in text.split_inclusive('\n').enumerate() {
        let line_number = index + 1;
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = line.strip_suffix('\r').unwrap_or(line);

        if let Some((marker, length, info, start, body)) = fence.as_mut() {
            if fence_closer(line, *marker, *length) {
                records.push(fence_record(
                    *marker,
                    *length,
                    info.clone(),
                    body,
                    *start,
                    line_number,
                ));
                fence = None;
                continue;
            }
            body.push_str(raw);
            markdown_line_records(line, line_number, &mut records);
            continue;
        }

        if let Some((marker, length, info)) = fence_opener(line) {
            fence = Some((marker, length, info, line_number, String::new()));
        }
        markdown_line_records(line, line_number, &mut records);
    }

    if let Some((marker, length, info, start, body)) = fence {
        let end = text.lines().count().max(start);
        records.push(fence_record(marker, length, info, &body, start, end));
        records.push(Record::issue(marker.to_string().repeat(length), start));
    }
    records
}

fn fence_opener(line: &str) -> Option<(char, usize, String)> {
    let content = line.trim_start_matches(' ');
    if line.len() - content.len() > 3 {
        return None;
    }
    let marker = content.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let length = content.chars().take_while(|value| *value == marker).count();
    (length >= 3).then(|| (marker, length, content[length..].trim().to_owned()))
}

fn fence_closer(line: &str, marker: char, length: usize) -> bool {
    let content = line.trim_start_matches(' ');
    if line.len() - content.len() > 3 {
        return false;
    }
    let run = content.chars().take_while(|value| *value == marker).count();
    run >= length && content[run..].trim().is_empty()
}

fn fence_record(
    marker: char,
    length: usize,
    info: String,
    body: &str,
    line: usize,
    end_line: usize,
) -> Record {
    let mut record = Record::new(
        "fence",
        vec![
            quoted("marker", marker.to_string().repeat(length)),
            quoted("info", info),
            field("digest", blake3::hash(body.as_bytes()).to_hex().to_string()),
        ],
        line,
    );
    record.end_line = end_line;
    record
}

fn markdown_line_records(line: &str, line_number: usize, records: &mut Vec<Record>) {
    if let Some((label, target)) = reference_definition(line) {
        records.push(Record::new(
            "reference-definition",
            vec![quoted("label", label), quoted("target", target.clone())],
            line_number,
        ));
        link_target_records(&target, line_number, records);
    }
    for target in inline_links(line) {
        records.push(Record::new(
            "link",
            vec![quoted("target", target.clone())],
            line_number,
        ));
        link_target_records(&target, line_number, records);
    }
    for value in inline_code_spans(line) {
        records.push(Record::new(
            "inline-code",
            vec![quoted("value", value.clone())],
            line_number,
        ));
        if repository_path(&value) {
            records.push(Record::new(
                "path",
                vec![quoted("value", value)],
                line_number,
            ));
        }
    }
    for value in requirement_tokens(line) {
        records.push(Record::new(
            "requirement",
            vec![quoted("value", value)],
            line_number,
        ));
    }
    for (state, value) in digest_tokens(line) {
        records.push(Record::new(
            "digest",
            vec![field("state", state), quoted("value", value)],
            line_number,
        ));
    }
}

fn reference_definition(line: &str) -> Option<(String, String)> {
    let line = line.trim_start_matches(' ');
    let close = line.find("]: ")?;
    (line.starts_with('[') && close > 1).then(|| {
        let label = line[1..close].to_owned();
        let target = line[close + 3..]
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches(['<', '>'])
            .to_owned();
        (label, target)
    })
}

fn inline_links(line: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find("](") {
        let candidate = &rest[open + 2..];
        let Some(close) = candidate.find(')') else {
            break;
        };
        let target = candidate[..close]
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches(['<', '>']);
        if !target.is_empty() {
            targets.push(target.to_owned());
        }
        rest = &candidate[close + 1..];
    }
    targets
}

fn inline_code_spans(line: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('`') {
        let marker = rest[start..]
            .chars()
            .take_while(|value| *value == '`')
            .count();
        let after = &rest[start + marker..];
        let close = "`".repeat(marker);
        let Some(end) = after.find(&close) else {
            break;
        };
        spans.push(after[..end].to_owned());
        rest = &after[end + marker..];
    }
    spans
}

fn requirement_tokens(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut values = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if index > 0 && token_byte(bytes[index - 1]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_uppercase() {
            index += 1;
        }
        let prefix_end = index;
        if prefix_end == start || bytes.get(index) != Some(&b'-') {
            index = start + 1;
            continue;
        }
        index += 1;
        let number_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index > number_start && (index == bytes.len() || !token_byte(bytes[index])) {
            values.push(line[start..index].to_owned());
        }
    }
    values
}

fn digest_tokens(line: &str) -> Vec<(&'static str, String)> {
    let bytes = line.as_bytes();
    let mut values = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_alphanumeric() {
            index += 1;
        }
        let word = &line[start..index];
        if matches!(word.len(), 40 | 64) && word.as_bytes().iter().all(u8::is_ascii_hexdigit) {
            values.push(("bare", word.to_owned()));
        }
        let mut claimed = None;
        for algorithm in ["sha1", "sha256", "blake3"] {
            if word.eq_ignore_ascii_case(algorithm) && bytes.get(index) == Some(&b':') {
                claimed = Some(algorithm);
                index += 1;
                break;
            }
        }
        let hex_start = index;
        while index < bytes.len() && bytes[index].is_ascii_hexdigit() {
            index += 1;
        }
        let length = index - hex_start;
        if matches!(length, 40 | 64) && (index == bytes.len() || !bytes[index].is_ascii_hexdigit())
        {
            let value = if let Some(algorithm) = claimed {
                format!("{algorithm}:{}", &line[hex_start..index])
            } else {
                line[hex_start..index].to_owned()
            };
            values.push((if claimed.is_some() { "claimed" } else { "bare" }, value));
        }
        if index == start {
            index += 1;
        }
    }
    values
}

fn token_byte(value: u8) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, b'_' | b'-')
}

fn link_target_records(target: &str, line: usize, records: &mut Vec<Record>) {
    if local_markdown_target(target) {
        records.push(Record::new(
            "spec-citation",
            vec![quoted("target", target)],
            line,
        ));
    }
    if repository_path(target) {
        records.push(Record::new("path", vec![quoted("value", target)], line));
    }
}

fn local_markdown_target(target: &str) -> bool {
    let path = target.split('#').next().unwrap_or_default();
    !target.contains("://")
        && !target.starts_with("//")
        && [".md", ".markdown"]
            .iter()
            .any(|extension| path.to_ascii_lowercase().ends_with(extension))
}

fn repository_path(value: &str) -> bool {
    value.contains('/') && !value.contains("://") && !value.starts_with('/')
}

#[derive(Clone)]
struct TsvRow {
    fields: Vec<String>,
    line: usize,
    key: String,
    occurrence: usize,
}

struct Tsv {
    header: Vec<String>,
    rows: Vec<TsvRow>,
}

fn analyze_tsv(path: &str, old: Option<&str>, new: Option<&str>) -> Result<String, String> {
    let old = old.map(parse_tsv).unwrap_or_else(empty_tsv);
    let new = new.map(parse_tsv).unwrap_or_else(empty_tsv);
    let mut output = Vec::new();
    output.push(tsv_line(
        "schema",
        String::new(),
        "",
        0,
        format!(
            "tsv path={path:?} kind=schema old={:?} new={:?}",
            old.header, new.header
        ),
    ));
    output.push(tsv_line(
        "key",
        String::new(),
        "",
        0,
        format!(
            "tsv path={path:?} kind=key key_basis=first-column old={} new={}",
            optional_quoted(old.header.first()),
            optional_quoted(new.header.first())
        ),
    ));
    tsv_issues(path, "old", &old, &mut output);
    tsv_issues(path, "new", &new, &mut output);

    let old_counts = key_counts(&old.rows);
    let new_counts = key_counts(&new.rows);
    let old_rows = row_map(&old.rows);
    let new_rows = row_map(&new.rows);
    let mut identities = old_rows.keys().cloned().collect::<Vec<_>>();
    identities.extend(
        new_rows
            .keys()
            .filter(|key| !old_rows.contains_key(*key))
            .cloned(),
    );
    identities.sort();
    for identity in identities {
        let old_row = old_rows.get(&identity);
        let new_row = new_rows.get(&identity);
        let ambiguous = old_counts.get(&identity.0).copied().unwrap_or(0) > 1
            || new_counts.get(&identity.0).copied().unwrap_or(0) > 1;
        match (old_row, new_row) {
            (Some(old_row), None) => {
                output.push(tsv_row_line(path, "removed", old_row, ambiguous, None))
            }
            (None, Some(new_row)) => {
                output.push(tsv_row_line(path, "added", new_row, ambiguous, None))
            }
            (Some(old_row), Some(new_row)) if old_row.fields != new_row.fields => {
                let columns = changed_columns(old_row, new_row, &old.header, &new.header);
                output.push(tsv_row_line(
                    path,
                    "modified",
                    new_row,
                    ambiguous,
                    Some(columns),
                ));
            }
            _ => {}
        }
    }
    output.sort();
    Ok(output
        .into_iter()
        .map(|line| line.text)
        .collect::<Vec<_>>()
        .join("\n"))
}

fn empty_tsv() -> Tsv {
    Tsv {
        header: Vec::new(),
        rows: Vec::new(),
    }
}

fn parse_tsv(text: &str) -> Tsv {
    let mut physical_rows = text
        .split_terminator('\n')
        .map(|row| row.strip_suffix('\r').unwrap_or(row));
    let Some(header) = physical_rows.next() else {
        return empty_tsv();
    };
    let header = header.split('\t').map(str::to_owned).collect::<Vec<_>>();
    let mut occurrences = BTreeMap::<String, usize>::new();
    let rows = physical_rows
        .enumerate()
        .map(|(index, row)| {
            let fields = row.split('\t').map(str::to_owned).collect::<Vec<_>>();
            let key = fields.first().cloned().unwrap_or_default();
            let occurrence = occurrences.entry(key.clone()).or_default();
            *occurrence += 1;
            TsvRow {
                fields,
                line: index + 2,
                key,
                occurrence: *occurrence,
            }
        })
        .collect();
    Tsv { header, rows }
}

fn optional_quoted(value: Option<&String>) -> String {
    value.map_or_else(|| "null".to_owned(), |value| format!("{value:?}"))
}

fn tsv_issues(path: &str, side: &str, tsv: &Tsv, output: &mut Vec<OutputLine>) {
    let mut headers = BTreeMap::<&str, usize>::new();
    for header in &tsv.header {
        *headers.entry(header).or_default() += 1;
    }
    for (name, count) in headers.into_iter().filter(|(_, count)| *count > 1) {
        output.push(tsv_line(
            "duplicate-header",
            name.to_owned(),
            "",
            0,
            format!(
                "tsv path={path:?} kind=duplicate-header side={side} name={name:?} count={count}"
            ),
        ));
    }
    for row in &tsv.rows {
        if row.fields.len() != tsv.header.len() {
            output.push(tsv_line(
                "row-width",
                row.line.to_string(),
                "",
                row.line,
                format!(
                    "tsv path={path:?} kind=row-width side={side} line={} expected={} actual={}",
                    row.line,
                    tsv.header.len(),
                    row.fields.len()
                ),
            ));
        }
    }
    for (key, count) in key_counts(&tsv.rows)
        .into_iter()
        .filter(|(_, count)| *count > 1)
    {
        output.push(tsv_line(
            "duplicate",
            key.clone(),
            "",
            0,
            format!(
                "tsv path={path:?} kind=duplicate side={side} key={key:?} count={count} identity=ambiguous"
            ),
        ));
    }
}

fn key_counts(rows: &[TsvRow]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for row in rows {
        *counts.entry(row.key.clone()).or_default() += 1;
    }
    counts
}

fn row_map(rows: &[TsvRow]) -> BTreeMap<(String, usize), &TsvRow> {
    rows.iter()
        .map(|row| ((row.key.clone(), row.occurrence), row))
        .collect()
}

fn changed_columns(
    old: &TsvRow,
    new: &TsvRow,
    old_header: &[String],
    new_header: &[String],
) -> Vec<String> {
    (0..old.fields.len().max(new.fields.len()))
        .filter(|&index| old.fields.get(index) != new.fields.get(index))
        .map(|index| column_name(index, old_header, new_header))
        .collect()
}

fn column_name(index: usize, old_header: &[String], new_header: &[String]) -> String {
    for header in [new_header, old_header] {
        if let Some(name) = header.get(index)
            && !name.is_empty()
            && header.iter().filter(|candidate| *candidate == name).count() == 1
        {
            return name.clone();
        }
    }
    format!("column_{}", index + 1)
}

fn tsv_row_line(
    path: &str,
    change: &str,
    row: &TsvRow,
    ambiguous: bool,
    columns: Option<Vec<String>>,
) -> OutputLine {
    let mut text = format!(
        "tsv path={path:?} change={change} kind=row key={:?} occurrence={}",
        row.key, row.occurrence
    );
    if ambiguous {
        text.push_str(" identity=ambiguous");
    }
    if let Some(columns) = columns {
        text.push_str(&format!(" columns={columns:?}"));
    }
    text.push_str(&format!(" line={}", row.line));
    tsv_line("row", row.key.clone(), change, row.line, text)
}

fn tsv_line(
    kind: &'static str,
    identity: String,
    change: &str,
    line: usize,
    text: String,
) -> OutputLine {
    OutputLine {
        kind,
        identity,
        change: change.to_owned(),
        line,
        text,
    }
}

fn diff_records(path: &str, old: &[Record], new: &[Record]) -> Vec<OutputLine> {
    let mut old_map: BTreeMap<RecordKey, Vec<Location>> = BTreeMap::new();
    let mut new_map: BTreeMap<RecordKey, Vec<Location>> = BTreeMap::new();
    for record in old.iter().filter(|record| !record.issue) {
        old_map
            .entry(record.key.clone())
            .or_default()
            .push(Location {
                line: record.line,
                end_line: record.end_line,
            });
    }
    for record in new.iter().filter(|record| !record.issue) {
        new_map
            .entry(record.key.clone())
            .or_default()
            .push(Location {
                line: record.line,
                end_line: record.end_line,
            });
    }
    old_map.values_mut().for_each(|locations| locations.sort());
    new_map.values_mut().for_each(|locations| locations.sort());
    let mut result = Vec::new();
    for (key, old_locations) in &old_map {
        let new_count = new_map.get(key).map_or(0, Vec::len);
        for location in old_locations.iter().skip(new_count) {
            let record = Record {
                key: key.clone(),
                line: location.line,
                end_line: location.end_line,
                issue: false,
            };
            result.push(output_line(path, Some("removed"), &record));
        }
    }
    for (key, new_locations) in &new_map {
        let old_count = old_map.get(key).map_or(0, Vec::len);
        for location in new_locations.iter().skip(old_count) {
            let record = Record {
                key: key.clone(),
                line: location.line,
                end_line: location.end_line,
                issue: false,
            };
            result.push(output_line(path, Some("added"), &record));
        }
    }
    result
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct OutputLine {
    kind: &'static str,
    identity: String,
    change: String,
    line: usize,
    text: String,
}

fn output_line(path: &str, change: Option<&str>, record: &Record) -> OutputLine {
    let mut text = format!("markdown path={path:?}");
    if let Some(change) = change {
        text.push_str(&format!(" change={change}"));
    }
    text.push_str(&format!(" kind={}", record.key.kind));
    for field in &record.key.fields {
        text.push(' ');
        text.push_str(field.name);
        text.push('=');
        if field.quoted {
            text.push_str(&format!("{:?}", field.value));
        } else {
            text.push_str(&field.value);
        }
    }
    if record.key.kind == "fence" {
        text.push_str(&format!(" lines={}-{}", record.line, record.end_line));
    } else {
        text.push_str(&format!(" line={}", record.line));
    }
    OutputLine {
        kind: record.key.kind,
        identity: format!("{:?}", record.key),
        change: change.unwrap_or_default().to_owned(),
        line: record.line,
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_reports_semantic_changes_and_claimed_digests() {
        let old_digest = "a".repeat(64);
        let new_digest = "b".repeat(64);
        let old = format!(
            "See [REQ-1](specs/old.md#req-1) and `src/old.rs`.\n[old-ref]: docs/old.md\n\n```rust\nsha256:{old_digest}\n```\n"
        );
        let new = format!(
            "See [REQ-2](specs/new.md#req-2) and `src/new.rs`.\n[new-ref]: docs/new.md\n\n```rust\nsha256:{new_digest}\n```\n"
        );

        let analysis = analyze("README.md", Some(&old), Some(&new)).unwrap();
        assert_eq!(analysis.kind, AnalyzerKind::Markdown);
        assert!(
            analysis
                .output
                .contains("change=removed kind=requirement value=\"REQ-1\"")
        );
        assert!(
            analysis
                .output
                .contains("change=added kind=requirement value=\"REQ-2\"")
        );
        assert!(
            analysis
                .output
                .contains("kind=spec-citation target=\"specs/new.md#req-2\"")
        );
        assert!(
            analysis
                .output
                .contains("kind=reference-definition label=\"new-ref\" target=\"docs/new.md\"")
        );
        assert!(analysis.output.contains("kind=path value=\"src/new.rs\""));
        assert!(analysis.output.contains("kind=digest state=claimed"));
        assert!(
            analysis
                .output
                .contains("kind=fence marker=\"```\" info=\"rust\"")
        );
        assert!(!analysis.output.contains('\t'));
    }

    #[test]
    fn markdown_reports_unclosed_fences_without_output_injection() {
        let analysis =
            analyze("notes.md", None, Some("REQ-9\n```text\nvalue\twith-tab\n")).unwrap();
        assert!(analysis.output.contains("issue=unclosed-fence"));
        assert!(analysis.output.contains("REQ-9"));
        assert!(!analysis.output.contains('\t'));
    }

    #[test]
    fn tsv_reports_schema_rows_duplicates_and_width_issues() {
        let old = "id\tvalue\na\told\na\tsecond\nb\tkeep\nd\tgone\n";
        let new = "id\tamount\na\tnew\na\tsecond\nc\tadded\nb\ttoo\twide\n";
        let analysis = analyze("fixtures/data.TSV", Some(old), Some(new)).unwrap();

        assert_eq!(analysis.kind, AnalyzerKind::Tsv);
        for expected in [
            "kind=schema old=[\"id\", \"value\"] new=[\"id\", \"amount\"]",
            "kind=key key_basis=first-column old=\"id\" new=\"id\"",
            "kind=duplicate side=old key=\"a\" count=2 identity=ambiguous",
            "kind=duplicate side=new key=\"a\" count=2 identity=ambiguous",
            "change=modified kind=row key=\"a\" occurrence=1 identity=ambiguous columns=[\"amount\"]",
            "change=removed kind=row key=\"d\" occurrence=1",
            "change=added kind=row key=\"c\" occurrence=1",
            "kind=row-width side=new line=5 expected=2 actual=3",
        ] {
            assert!(
                analysis.output.contains(expected),
                "missing {expected}: {}",
                analysis.output
            );
        }
    }

    #[test]
    fn tsv_reports_duplicate_headers() {
        let analysis = analyze("data.tsv", None, Some("id\tid\na\t1\n")).unwrap();
        assert!(
            analysis
                .output
                .contains("kind=duplicate-header side=new name=\"id\" count=2")
        );
    }

    #[test]
    fn generic_text_has_no_semantic_output() {
        let analysis = analyze("config.toml", Some("old\n"), Some("new\n")).unwrap();
        assert_eq!(analysis.kind, AnalyzerKind::Generic);
        assert!(analysis.output.is_empty());
    }
}
