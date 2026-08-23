use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::evidence::CoverageFormat;
use rmcp::serde_json::Value;

const COVERAGE_OBSERVATION_LIMIT: usize = 100_000;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CoverageObservation {
    pub path: Option<String>,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub execution_count: u64,
    pub context: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum BranchObservationKind {
    TrueOutcome,
    FalseOutcome,
    Arc,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BranchObservation {
    pub path: Option<String>,
    pub start_line: i64,
    pub start_column: u32,
    pub end_line: i64,
    pub end_column: u32,
    pub target_line: Option<i64>,
    pub kind: BranchObservationKind,
    pub execution_count: u64,
    pub context: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedCoverage {
    pub format: CoverageFormat,
    pub regions: Vec<CoverageObservation>,
    pub branches: Vec<BranchObservation>,
    pub external_paths: u32,
}

pub fn parse_coverage(
    format: CoverageFormat,
    bytes: &[u8],
    worktree_root: &Path,
    cancelled: &AtomicBool,
) -> Result<ParsedCoverage, String> {
    check_cancelled(cancelled)?;
    match format {
        CoverageFormat::Llvm => parse_llvm(bytes, worktree_root, cancelled),
        CoverageFormat::CoveragePy => parse_coverage_py(bytes, worktree_root, cancelled),
    }
}

type RegionKey = (Option<String>, u32, u32, u32, u32, Option<String>);
type BranchKey = (
    Option<String>,
    i64,
    u32,
    i64,
    u32,
    Option<i64>,
    BranchObservationKind,
    Option<String>,
);

fn parse_llvm(
    bytes: &[u8],
    worktree_root: &Path,
    cancelled: &AtomicBool,
) -> Result<ParsedCoverage, String> {
    let root: Value = rmcp::serde_json::from_slice(bytes)
        .map_err(|_| "LLVM coverage report is invalid".to_owned())?;
    let object = root
        .as_object()
        .ok_or_else(|| "LLVM coverage report is invalid".to_owned())?;
    if object.get("type").and_then(Value::as_str) != Some("llvm.coverage.json.export") {
        return Err("LLVM coverage report type is unsupported".into());
    }
    let version = object
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| "LLVM coverage report version is invalid".to_owned())?;
    let mut version_parts = version.split('.');
    let major = version_parts
        .next()
        .and_then(|part| part.parse::<u32>().ok());
    if !matches!(major, Some(2 | 3))
        || version_parts
            .next()
            .and_then(|part| part.parse::<u32>().ok())
            .is_none()
        || version_parts
            .next()
            .and_then(|part| part.parse::<u32>().ok())
            .is_none()
        || version_parts.next().is_some()
    {
        return Err("LLVM coverage report version is unsupported".into());
    }
    let data = object
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "LLVM coverage report data is invalid".to_owned())?;
    let normalized_root = normalize_absolute(worktree_root)
        .ok_or_else(|| "authorized worktree path is invalid".to_owned())?;
    let mut regions = BTreeMap::<RegionKey, u64>::new();
    let mut branches = BTreeMap::<BranchKey, u64>::new();
    let mut external = BTreeSet::new();
    let mut observations = 0;

    for export in data {
        check_cancelled(cancelled)?;
        let export = export
            .as_object()
            .ok_or_else(|| "LLVM coverage export block is invalid".to_owned())?;
        if let Some(functions) = export.get("functions") {
            let functions = functions
                .as_array()
                .ok_or_else(|| "LLVM coverage functions are invalid".to_owned())?;
            for function in functions {
                check_cancelled(cancelled)?;
                let function = function
                    .as_object()
                    .ok_or_else(|| "LLVM coverage function is invalid".to_owned())?;
                let filenames = function
                    .get("filenames")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "LLVM coverage filenames are invalid".to_owned())?;
                let filenames = filenames
                    .iter()
                    .map(|filename| {
                        filename
                            .as_str()
                            .ok_or_else(|| "LLVM coverage filename is invalid".to_owned())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let tuples = function
                    .get("regions")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "LLVM coverage regions are invalid".to_owned())?;
                for tuple in tuples {
                    check_cancelled(cancelled)?;
                    let tuple = exact_tuple(tuple, 8, "LLVM coverage region")?;
                    let (start_line, start_column, end_line, end_column) = coordinates(tuple)?;
                    let count = integer(tuple, 4, "LLVM coverage count")?;
                    let file_id = index(tuple, 5, "LLVM coverage file ID")?;
                    let expanded_file_id = index(tuple, 6, "LLVM coverage expanded file ID")?;
                    let kind = u32::try_from(integer(tuple, 7, "LLVM coverage region kind")?)
                        .map_err(|_| "LLVM coverage region kind exceeds range".to_owned())?;
                    let filename = *filenames
                        .get(file_id)
                        .ok_or_else(|| "LLVM coverage file ID is invalid".to_owned())?;
                    if filenames.get(expanded_file_id).is_none() {
                        return Err("LLVM coverage expanded file ID is invalid".into());
                    }
                    match kind {
                        0 => {
                            let path = coverage_path(filename, &normalized_root, &mut external);
                            add_count(
                                &mut regions,
                                &mut observations,
                                cancelled,
                                (path, start_line, start_column, end_line, end_column, None),
                                count,
                            )?;
                        }
                        1..=6 => {}
                        _ => return Err("LLVM coverage region kind is unsupported".into()),
                    }
                }
            }
        }
        if let Some(files) = export.get("files") {
            let files = files
                .as_array()
                .ok_or_else(|| "LLVM coverage files are invalid".to_owned())?;
            for file in files {
                check_cancelled(cancelled)?;
                let file = file
                    .as_object()
                    .ok_or_else(|| "LLVM coverage file is invalid".to_owned())?;
                let filename = file
                    .get("filename")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "LLVM coverage filename is invalid".to_owned())?;
                let Some(tuples) = file.get("branches") else {
                    continue;
                };
                let tuples = tuples
                    .as_array()
                    .ok_or_else(|| "LLVM coverage branches are invalid".to_owned())?;
                for tuple in tuples {
                    check_cancelled(cancelled)?;
                    let tuple = exact_tuple(tuple, 9, "LLVM coverage branch")?;
                    let (start_line, start_column, end_line, end_column) = coordinates(tuple)?;
                    let true_count = integer(tuple, 4, "LLVM coverage true count")?;
                    let false_count = integer(tuple, 5, "LLVM coverage false count")?;
                    let _file_id = u32::try_from(integer(tuple, 6, "LLVM coverage file ID")?)
                        .map_err(|_| "LLVM coverage file ID exceeds range".to_owned())?;
                    let _expanded_file_id =
                        u32::try_from(integer(tuple, 7, "LLVM coverage expanded file ID")?)
                            .map_err(|_| {
                                "LLVM coverage expanded file ID exceeds range".to_owned()
                            })?;
                    let kind = u32::try_from(integer(tuple, 8, "LLVM coverage branch kind")?)
                        .map_err(|_| "LLVM coverage branch kind exceeds range".to_owned())?;
                    if !matches!(kind, 4 | 6) {
                        return Err("LLVM coverage branch kind is unsupported".into());
                    }
                    let path = coverage_path(filename, &normalized_root, &mut external);
                    for (kind, count) in [
                        (BranchObservationKind::TrueOutcome, true_count),
                        (BranchObservationKind::FalseOutcome, false_count),
                    ] {
                        add_count(
                            &mut branches,
                            &mut observations,
                            cancelled,
                            (
                                path.clone(),
                                i64::from(start_line),
                                start_column,
                                i64::from(end_line),
                                end_column,
                                None,
                                kind,
                                None,
                            ),
                            count,
                        )?;
                    }
                }
            }
        }
    }
    if regions.is_empty() {
        return Err("LLVM coverage report has no region data".into());
    }
    Ok(ParsedCoverage {
        format: CoverageFormat::Llvm,
        regions: regions
            .into_iter()
            .map(
                |((path, start_line, start_column, end_line, end_column, context), count)| {
                    CoverageObservation {
                        path,
                        start_line,
                        start_column,
                        end_line,
                        end_column,
                        execution_count: count,
                        context,
                    }
                },
            )
            .collect(),
        branches: branches
            .into_iter()
            .map(
                |(
                    (
                        path,
                        start_line,
                        start_column,
                        end_line,
                        end_column,
                        target_line,
                        kind,
                        context,
                    ),
                    count,
                )| BranchObservation {
                    path,
                    start_line,
                    start_column,
                    end_line,
                    end_column,
                    target_line,
                    kind,
                    execution_count: count,
                    context,
                },
            )
            .collect(),
        external_paths: u32::try_from(external.len())
            .map_err(|_| "LLVM coverage has too many external paths".to_owned())?,
    })
}

fn parse_coverage_py(
    bytes: &[u8],
    worktree_root: &Path,
    cancelled: &AtomicBool,
) -> Result<ParsedCoverage, String> {
    let root: Value = rmcp::serde_json::from_slice(bytes)
        .map_err(|_| "Coverage.py report is invalid".to_owned())?;
    let object = root
        .as_object()
        .ok_or_else(|| "Coverage.py report is invalid".to_owned())?;
    let meta = object
        .get("meta")
        .and_then(Value::as_object)
        .ok_or_else(|| "Coverage.py metadata is invalid".to_owned())?;
    if meta.get("format").and_then(Value::as_u64) != Some(3) {
        return Err("Coverage.py report format is unsupported".into());
    }
    let version = meta
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| "Coverage.py package version is invalid".to_owned())?;
    validate_context(version, "Coverage.py package version")?;
    let files = object
        .get("files")
        .and_then(Value::as_object)
        .ok_or_else(|| "Coverage.py files are invalid".to_owned())?;
    let normalized_root = normalize_absolute(worktree_root)
        .ok_or_else(|| "authorized worktree path is invalid".to_owned())?;
    let mut regions = BTreeMap::<RegionKey, u64>::new();
    let mut branches = BTreeMap::<BranchKey, u64>::new();
    let mut external = BTreeSet::new();
    let mut observations = 0;

    for (filename, file) in files {
        check_cancelled(cancelled)?;
        let file = file
            .as_object()
            .ok_or_else(|| "Coverage.py file entry is invalid".to_owned())?;
        let contexts = match file.get("contexts") {
            Some(value) => {
                let values = value
                    .as_object()
                    .ok_or_else(|| "Coverage.py contexts are invalid".to_owned())?;
                let mut contexts = BTreeMap::<u32, Vec<Option<String>>>::new();
                for (line, names) in values {
                    check_cancelled(cancelled)?;
                    let parsed = line
                        .parse::<u32>()
                        .map_err(|_| "Coverage.py context line is invalid".to_owned())?;
                    if parsed == 0 || parsed.to_string() != *line {
                        return Err("Coverage.py context line is invalid".into());
                    }
                    let names = names
                        .as_array()
                        .ok_or_else(|| "Coverage.py context list is invalid".to_owned())?;
                    let mut unique = BTreeSet::new();
                    let mut parsed_names = Vec::with_capacity(names.len());
                    for name in names {
                        check_cancelled(cancelled)?;
                        let name = name
                            .as_str()
                            .ok_or_else(|| "Coverage.py context is invalid".to_owned())?;
                        let name = if name.is_empty() {
                            None
                        } else {
                            validate_context(name, "Coverage.py context")?;
                            Some(name.to_owned())
                        };
                        if !unique.insert(name.clone()) {
                            return Err("Coverage.py context is duplicated".into());
                        }
                        parsed_names.push(name);
                    }
                    contexts.insert(parsed, parsed_names);
                }
                contexts
            }
            None => BTreeMap::new(),
        };
        let path = coverage_path(filename, &normalized_root, &mut external);
        for (field, count) in [("executed_lines", 1), ("missing_lines", 0)] {
            let Some(lines) = file.get(field) else {
                continue;
            };
            let lines = lines
                .as_array()
                .ok_or_else(|| format!("Coverage.py {field} are invalid"))?;
            for line in lines {
                check_cancelled(cancelled)?;
                let line = value_positive_u32(line, "Coverage.py line")?;
                match contexts.get(&line).filter(|contexts| !contexts.is_empty()) {
                    Some(contexts) => {
                        for context in contexts {
                            check_cancelled(cancelled)?;
                            insert_coverage_py(
                                &mut regions,
                                &mut observations,
                                cancelled,
                                (path.clone(), line, 0, line, 0, context.clone()),
                                count,
                            )?;
                        }
                    }
                    None => insert_coverage_py(
                        &mut regions,
                        &mut observations,
                        cancelled,
                        (path.clone(), line, 0, line, 0, None),
                        count,
                    )?,
                }
            }
        }
        for (field, count) in [("executed_branches", 1), ("missing_branches", 0)] {
            let Some(arcs) = file.get(field) else {
                continue;
            };
            let arcs = arcs
                .as_array()
                .ok_or_else(|| format!("Coverage.py {field} are invalid"))?;
            for arc in arcs {
                check_cancelled(cancelled)?;
                let arc = exact_tuple(arc, 2, "Coverage.py branch")?;
                let start_line = value_nonzero_i64(&arc[0], "Coverage.py branch source")?;
                let target_line = value_nonzero_i64(&arc[1], "Coverage.py branch target")?;
                insert_coverage_py(
                    &mut branches,
                    &mut observations,
                    cancelled,
                    (
                        path.clone(),
                        start_line,
                        0,
                        start_line,
                        0,
                        Some(target_line),
                        BranchObservationKind::Arc,
                        None,
                    ),
                    count,
                )?;
            }
        }
    }
    if regions.is_empty() {
        return Err("Coverage.py report has no executable line data".into());
    }
    Ok(ParsedCoverage {
        format: CoverageFormat::CoveragePy,
        regions: regions
            .into_iter()
            .map(
                |((path, start_line, start_column, end_line, end_column, context), count)| {
                    CoverageObservation {
                        path,
                        start_line,
                        start_column,
                        end_line,
                        end_column,
                        execution_count: count,
                        context,
                    }
                },
            )
            .collect(),
        branches: branches
            .into_iter()
            .map(
                |(
                    (
                        path,
                        start_line,
                        start_column,
                        end_line,
                        end_column,
                        target_line,
                        kind,
                        context,
                    ),
                    count,
                )| BranchObservation {
                    path,
                    start_line,
                    start_column,
                    end_line,
                    end_column,
                    target_line,
                    kind,
                    execution_count: count,
                    context,
                },
            )
            .collect(),
        external_paths: u32::try_from(external.len())
            .map_err(|_| "Coverage.py report has too many external paths".to_owned())?,
    })
}

fn exact_tuple<'a>(value: &'a Value, len: usize, kind: &str) -> Result<&'a [Value], String> {
    let tuple = value
        .as_array()
        .ok_or_else(|| format!("{kind} tuple is invalid"))?;
    if tuple.len() != len {
        return Err(format!("{kind} tuple length is invalid"));
    }
    Ok(tuple)
}

fn coordinates(tuple: &[Value]) -> Result<(u32, u32, u32, u32), String> {
    let start_line = positive_u32(tuple, 0, "coverage start line")?;
    let start_column = positive_u32(tuple, 1, "coverage start column")?;
    let end_line = positive_u32(tuple, 2, "coverage end line")?;
    let end_column = positive_u32(tuple, 3, "coverage end column")?;
    if (end_line, end_column) < (start_line, start_column) {
        return Err("coverage range is invalid".into());
    }
    Ok((start_line, start_column, end_line, end_column))
}

fn integer(tuple: &[Value], index: usize, kind: &str) -> Result<u64, String> {
    tuple[index]
        .as_u64()
        .ok_or_else(|| format!("{kind} is invalid"))
}

fn positive_u32(tuple: &[Value], index: usize, kind: &str) -> Result<u32, String> {
    value_positive_u32(&tuple[index], kind)
}

fn value_positive_u32(value: &Value, kind: &str) -> Result<u32, String> {
    let value = u32::try_from(value.as_u64().ok_or_else(|| format!("{kind} is invalid"))?)
        .map_err(|_| format!("{kind} exceeds range"))?;
    if value == 0 {
        Err(format!("{kind} is invalid"))
    } else {
        Ok(value)
    }
}

fn value_nonzero_i64(value: &Value, kind: &str) -> Result<i64, String> {
    let value = value.as_i64().ok_or_else(|| format!("{kind} is invalid"))?;
    if value == 0 {
        Err(format!("{kind} is invalid"))
    } else {
        Ok(value)
    }
}

fn validate_context(value: &str, kind: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
        Err(format!("{kind} is invalid"))
    } else {
        Ok(())
    }
}

fn index(tuple: &[Value], index: usize, kind: &str) -> Result<usize, String> {
    usize::try_from(integer(tuple, index, kind)?).map_err(|_| format!("{kind} exceeds range"))
}

fn add_count<K: Ord>(
    values: &mut BTreeMap<K, u64>,
    observations: &mut usize,
    cancelled: &AtomicBool,
    key: K,
    count: u64,
) -> Result<(), String> {
    charge_observation(observations, cancelled)?;
    let total = values.entry(key).or_default();
    *total = total
        .checked_add(count)
        .ok_or_else(|| "coverage count overflow".to_owned())?;
    Ok(())
}

fn insert_coverage_py<K: Ord>(
    values: &mut BTreeMap<K, u64>,
    observations: &mut usize,
    cancelled: &AtomicBool,
    key: K,
    count: u64,
) -> Result<(), String> {
    charge_observation(observations, cancelled)?;
    if values.insert(key, count).is_some() {
        Err("Coverage.py observation is duplicated or contradictory".into())
    } else {
        Ok(())
    }
}

fn charge_observation(observations: &mut usize, cancelled: &AtomicBool) -> Result<(), String> {
    check_cancelled(cancelled)?;
    if *observations >= COVERAGE_OBSERVATION_LIMIT {
        return Err("coverage report has too many observations".into());
    }
    *observations += 1;
    Ok(())
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("coverage parsing cancelled".into())
    } else {
        Ok(())
    }
}

fn coverage_path(
    filename: &str,
    worktree_root: &Path,
    external: &mut BTreeSet<String>,
) -> Option<String> {
    let path = Path::new(filename);
    let absolute = if path.is_absolute() {
        normalize_absolute(path)
    } else {
        normalize_absolute(&worktree_root.join(path))
    };
    let relative = absolute
        .as_deref()
        .and_then(|path| path.strip_prefix(worktree_root).ok())
        .and_then(safe_relative_path);
    if relative.is_none() {
        external.insert(filename.to_owned());
    }
    relative
}

fn normalize_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
        }
    }
    Some(normalized)
}

fn safe_relative_path(path: &Path) -> Option<String> {
    let value = path.to_str()?;
    if value.is_empty()
        || value.len() > 1024
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        None
    } else {
        Some(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use rmcp::serde_json::{Value, json};

    use super::*;

    fn parse_report(
        format: CoverageFormat,
        bytes: &[u8],
        worktree_root: &Path,
    ) -> Result<ParsedCoverage, String> {
        parse_coverage(format, bytes, worktree_root, &AtomicBool::new(false))
    }

    fn llvm_report(version: &str) -> Value {
        json!({
            "type": "llvm.coverage.json.export",
            "version": version,
            "data": [
                {
                    "functions": [
                        {
                            "name": "ignored",
                            "count": 5,
                            "filenames": [
                                "src/lib.rs",
                                "/repo/src/absolute.rs",
                                "/outside/private.rs"
                            ],
                            "regions": [
                                [2, 1, 3, 4, 5, 0, 0, 0],
                                [4, 1, 4, 2, 0, 1, 1, 0],
                                [5, 1, 5, 2, 1, 2, 2, 0]
                            ]
                        }
                    ],
                    "files": [
                        {
                            "filename": "src/lib.rs",
                            "branches": [
                                [9, 1, 9, 2, 3, 0, 0, 0, 4],
                                [10, 1, 10, 2, 0, 4, 0, 0, 4]
                            ],
                            "segments": [],
                            "expansions": [],
                            "summary": {}
                        }
                    ],
                    "totals": {}
                },
                {
                    "functions": [
                        {
                            "name": "also ignored",
                            "count": 2,
                            "filenames": ["src/lib.rs"],
                            "regions": [[2, 1, 3, 4, 2, 0, 0, 0]]
                        }
                    ],
                    "files": [
                        {
                            "filename": "src/lib.rs",
                            "branches": [[9, 1, 9, 2, 2, 1, 0, 0, 4]]
                        }
                    ],
                    "totals": {}
                }
            ]
        })
    }

    #[test]
    fn coverage_py_rejects_more_than_one_hundred_thousand_observations() {
        let report = |regions: u32, branches: u32| {
            let executed_lines = (1..=regions).collect::<Vec<_>>();
            let executed_branches = (1..=branches)
                .map(|line| [i64::from(line), i64::from(line) + 1])
                .collect::<Vec<_>>();
            json!({
                "meta": {"format": 3, "version": "7"},
                "files": {
                    "src/lib.rs": {
                        "executed_lines": executed_lines,
                        "executed_branches": executed_branches
                    }
                }
            })
            .to_string()
        };

        assert!(
            parse_coverage_py(
                report(50_000, 50_000).as_bytes(),
                Path::new("/repo"),
                &AtomicBool::new(false),
            )
            .is_ok()
        );
        assert!(
            parse_coverage_py(
                report(50_000, 50_001).as_bytes(),
                Path::new("/repo"),
                &AtomicBool::new(false),
            )
            .is_err_and(|error| error == "coverage report has too many observations")
        );
    }

    #[test]
    fn llvm_rejects_more_than_one_hundred_thousand_observations() {
        let report = |branches: u32| {
            let branches = (1..=branches)
                .map(|line| [line, 1, line, 2, 1, 0, 0, 0, 4])
                .collect::<Vec<_>>();
            json!({
                "type": "llvm.coverage.json.export",
                "version": "2.0.1",
                "data": [{
                    "functions": [{
                        "filenames": ["src/lib.rs"],
                        "regions": [
                            [1, 1, 1, 2, 1, 0, 0, 0],
                            [2, 1, 2, 2, 1, 0, 0, 0]
                        ]
                    }],
                    "files": [{"filename": "src/lib.rs", "branches": branches}]
                }]
            })
            .to_string()
        };

        assert!(
            parse_llvm(
                report(49_999).as_bytes(),
                Path::new("/repo"),
                &AtomicBool::new(false),
            )
            .is_ok()
        );
        assert!(
            parse_llvm(
                report(50_000).as_bytes(),
                Path::new("/repo"),
                &AtomicBool::new(false),
            )
            .is_err_and(|error| error == "coverage report has too many observations")
        );
    }

    #[test]
    fn coverage_decode_honors_preexisting_cancellation() {
        let report =
            br#"{"meta":{"format":3,"version":"7"},"files":{"src/lib.rs":{"executed_lines":[1]}}}"#;
        let error = parse_coverage(
            CoverageFormat::CoveragePy,
            report,
            Path::new("/repo"),
            &std::sync::atomic::AtomicBool::new(true),
        )
        .unwrap_err();
        assert_eq!(error, "coverage parsing cancelled");
    }

    #[test]
    fn llvm_v2_v3_decode_regions_branches_paths_and_fold_duplicates() {
        for version in ["2.0.1", "3.0.0"] {
            let parsed = parse_report(
                CoverageFormat::Llvm,
                llvm_report(version).to_string().as_bytes(),
                Path::new("/repo"),
            )
            .unwrap();

            assert_eq!(parsed.format, CoverageFormat::Llvm);
            assert_eq!(parsed.external_paths, 1);
            assert_eq!(
                parsed.regions,
                vec![
                    CoverageObservation {
                        path: None,
                        start_line: 5,
                        start_column: 1,
                        end_line: 5,
                        end_column: 2,
                        execution_count: 1,
                        context: None,
                    },
                    CoverageObservation {
                        path: Some("src/absolute.rs".into()),
                        start_line: 4,
                        start_column: 1,
                        end_line: 4,
                        end_column: 2,
                        execution_count: 0,
                        context: None,
                    },
                    CoverageObservation {
                        path: Some("src/lib.rs".into()),
                        start_line: 2,
                        start_column: 1,
                        end_line: 3,
                        end_column: 4,
                        execution_count: 7,
                        context: None,
                    },
                ]
            );
            assert_eq!(
                parsed.branches,
                vec![
                    BranchObservation {
                        path: Some("src/lib.rs".into()),
                        start_line: 9,
                        start_column: 1,
                        end_line: 9,
                        end_column: 2,
                        target_line: None,
                        kind: BranchObservationKind::TrueOutcome,
                        execution_count: 5,
                        context: None,
                    },
                    BranchObservation {
                        path: Some("src/lib.rs".into()),
                        start_line: 9,
                        start_column: 1,
                        end_line: 9,
                        end_column: 2,
                        target_line: None,
                        kind: BranchObservationKind::FalseOutcome,
                        execution_count: 1,
                        context: None,
                    },
                    BranchObservation {
                        path: Some("src/lib.rs".into()),
                        start_line: 10,
                        start_column: 1,
                        end_line: 10,
                        end_column: 2,
                        target_line: None,
                        kind: BranchObservationKind::TrueOutcome,
                        execution_count: 0,
                        context: None,
                    },
                    BranchObservation {
                        path: Some("src/lib.rs".into()),
                        start_line: 10,
                        start_column: 1,
                        end_line: 10,
                        end_column: 2,
                        target_line: None,
                        kind: BranchObservationKind::FalseOutcome,
                        execution_count: 4,
                        context: None,
                    },
                ]
            );
            assert!(
                parsed
                    .regions
                    .iter()
                    .filter_map(|region| region.path.as_deref())
                    .all(|path| !path.contains("outside"))
            );
        }
    }

    #[test]
    fn llvm_rejects_malformed_tuples_counts_coordinates_and_file_ids() {
        let mut cases = Vec::new();
        for tuple in [
            json!([1, 1, 1, 2, 1, 0, 0]),
            json!([1, 1, 1, 2, -1, 0, 0, 0]),
            json!([1, 1, 1, 2, 1.5, 0, 0, 0]),
            json!([0, 1, 1, 2, 1, 0, 0, 0]),
            json!([2, 1, 1, 2, 1, 0, 0, 0]),
            json!([1, 1, 1, 2, 1, 2, 0, 0]),
        ] {
            let mut report = llvm_report("2.0.1");
            report["data"] = json!([{
                "functions": [{"name":"f", "filenames":["src/lib.rs"], "regions":[tuple]}],
                "files": []
            }]);
            cases.push(report);
        }
        let mut bad_branch = llvm_report("2.0.1");
        bad_branch["data"] = json!([{
            "functions": [{"name":"f", "filenames":["src/lib.rs"], "regions":[[1,1,1,2,1,0,0,0]]}],
            "files": [{"filename":"src/lib.rs", "branches":[[1,1,1,2,1,0,0,0]]}]
        }]);
        cases.push(bad_branch);

        for report in cases {
            assert!(
                parse_report(
                    CoverageFormat::Llvm,
                    report.to_string().as_bytes(),
                    Path::new("/repo")
                )
                .is_err(),
                "accepted malformed report: {report}"
            );
        }
    }

    #[test]
    fn llvm_rejects_wrong_type_version_and_summary_only_json() {
        for report in [
            json!({"type":"other", "version":"2.0.1", "data":[]}),
            json!({"type":"llvm.coverage.json.export", "version":"1.0.0", "data":[]}),
            json!({"type":"llvm.coverage.json.export", "version":"4.0.0", "data":[]}),
            json!({
                "type":"llvm.coverage.json.export",
                "version":"2.0.1",
                "data":[{"files":[{"filename":"src/lib.rs", "summary":{}}], "totals":{}}]
            }),
        ] {
            assert!(
                parse_report(
                    CoverageFormat::Llvm,
                    report.to_string().as_bytes(),
                    Path::new("/repo")
                )
                .is_err()
            );
        }
    }

    #[test]
    fn llvm_imports_only_code_regions_and_valid_branch_region_kinds() {
        let report = json!({
            "type": "llvm.coverage.json.export",
            "version": "3.0.0",
            "data": [{
                "functions": [{
                    "name": "ignored",
                    "filenames": ["src/lib.rs"],
                    "regions": [
                        [1, 1, 1, 2, 1, 0, 0, 0],
                        [2, 1, 2, 2, 1, 0, 0, 1],
                        [3, 1, 3, 2, 1, 0, 0, 2],
                        [4, 1, 4, 2, 1, 0, 0, 3],
                        [5, 1, 5, 2, 1, 0, 0, 4],
                        [6, 1, 6, 2, 1, 0, 0, 5],
                        [7, 1, 7, 2, 1, 0, 0, 6]
                    ]
                }],
                "files": [{
                    "filename": "src/lib.rs",
                    "branches": [
                        [8, 1, 8, 2, 1, 0, 0, 0, 4],
                        [9, 1, 9, 2, 1, 0, 0, 0, 6]
                    ]
                }]
            }]
        });
        let parsed = parse_report(
            CoverageFormat::Llvm,
            report.to_string().as_bytes(),
            Path::new("/repo"),
        )
        .unwrap();

        assert_eq!(
            parsed
                .regions
                .iter()
                .map(|region| region.start_line)
                .collect::<Vec<_>>(),
            [1]
        );
        assert_eq!(
            parsed
                .branches
                .iter()
                .map(|branch| branch.start_line)
                .collect::<Vec<_>>(),
            [8, 8, 9, 9]
        );

        for (field, tuple) in [
            ("regions", json!([1, 1, 1, 2, 1, 0, 0, 7])),
            ("branches", json!([1, 1, 1, 2, 1, 0, 0, 0, 0])),
        ] {
            let mut malformed = report.clone();
            if field == "regions" {
                malformed["data"][0]["functions"][0]["regions"] = json!([tuple]);
            } else {
                malformed["data"][0]["files"][0]["branches"] = json!([tuple]);
            }
            assert!(
                parse_report(
                    CoverageFormat::Llvm,
                    malformed.to_string().as_bytes(),
                    Path::new("/repo"),
                )
                .is_err(),
                "accepted unsupported LLVM {field} kind"
            );
        }
    }

    fn coverage_py_report() -> Value {
        json!({
            "meta": {
                "format": 3,
                "version": "coverage.py 9.1 arbitrary package text",
                "timestamp": "ignored",
                "branch_coverage": true,
                "show_contexts": true
            },
            "files": {
                "pkg/a.py": {
                    "executed_lines": [2, 3, 4],
                    "missing_lines": [5],
                    "excluded_lines": [],
                    "executed_branches": [[2, 3]],
                    "missing_branches": [[2, 5]],
                    "contexts": {
                        "2": ["test_one"],
                        "3": ["test_two", "test_one"],
                        "4": []
                    },
                    "summary": {}
                },
                "/repo/pkg/b.py": {
                    "executed_lines": [7],
                    "missing_lines": [],
                    "executed_branches": [],
                    "missing_branches": [],
                    "summary": {}
                },
                "/external/private.py": {
                    "executed_lines": [8],
                    "missing_lines": [],
                    "summary": {}
                }
            },
            "totals": {}
        })
    }

    #[test]
    fn coverage_py_v3_decodes_context_lines_and_run_scoped_arcs() {
        let parsed = parse_report(
            CoverageFormat::CoveragePy,
            coverage_py_report().to_string().as_bytes(),
            Path::new("/repo"),
        )
        .unwrap();

        assert_eq!(parsed.format, CoverageFormat::CoveragePy);
        assert_eq!(parsed.external_paths, 1);
        assert_eq!(
            parsed.regions,
            vec![
                CoverageObservation {
                    path: None,
                    start_line: 8,
                    start_column: 0,
                    end_line: 8,
                    end_column: 0,
                    execution_count: 1,
                    context: None,
                },
                CoverageObservation {
                    path: Some("pkg/a.py".into()),
                    start_line: 2,
                    start_column: 0,
                    end_line: 2,
                    end_column: 0,
                    execution_count: 1,
                    context: Some("test_one".into()),
                },
                CoverageObservation {
                    path: Some("pkg/a.py".into()),
                    start_line: 3,
                    start_column: 0,
                    end_line: 3,
                    end_column: 0,
                    execution_count: 1,
                    context: Some("test_one".into()),
                },
                CoverageObservation {
                    path: Some("pkg/a.py".into()),
                    start_line: 3,
                    start_column: 0,
                    end_line: 3,
                    end_column: 0,
                    execution_count: 1,
                    context: Some("test_two".into()),
                },
                CoverageObservation {
                    path: Some("pkg/a.py".into()),
                    start_line: 4,
                    start_column: 0,
                    end_line: 4,
                    end_column: 0,
                    execution_count: 1,
                    context: None,
                },
                CoverageObservation {
                    path: Some("pkg/a.py".into()),
                    start_line: 5,
                    start_column: 0,
                    end_line: 5,
                    end_column: 0,
                    execution_count: 0,
                    context: None,
                },
                CoverageObservation {
                    path: Some("pkg/b.py".into()),
                    start_line: 7,
                    start_column: 0,
                    end_line: 7,
                    end_column: 0,
                    execution_count: 1,
                    context: None,
                },
            ]
        );
        assert_eq!(
            parsed.branches,
            vec![
                BranchObservation {
                    path: Some("pkg/a.py".into()),
                    start_line: 2,
                    start_column: 0,
                    end_line: 2,
                    end_column: 0,
                    target_line: Some(3),
                    kind: BranchObservationKind::Arc,
                    execution_count: 1,
                    context: None,
                },
                BranchObservation {
                    path: Some("pkg/a.py".into()),
                    start_line: 2,
                    start_column: 0,
                    end_line: 2,
                    end_column: 0,
                    target_line: Some(5),
                    kind: BranchObservationKind::Arc,
                    execution_count: 0,
                    context: None,
                },
            ]
        );
        assert!(
            parsed
                .branches
                .iter()
                .all(|branch| branch.context.is_none())
        );
    }

    #[test]
    fn coverage_py_accepts_empty_run_context_and_signed_entry_exit_arcs() {
        let report = json!({
            "meta": {"format": 3, "version": "7.0"},
            "files": {"pkg/a.py": {
                "executed_lines": [8],
                "missing_lines": [],
                "executed_branches": [[-1, 8], [8, -1]],
                "missing_branches": [],
                "contexts": {"8": [""]}
            }}
        });
        let parsed = parse_report(
            CoverageFormat::CoveragePy,
            report.to_string().as_bytes(),
            Path::new("/repo"),
        )
        .unwrap();

        assert_eq!(parsed.regions.len(), 1);
        assert_eq!(parsed.regions[0].context, None);
        assert_eq!(parsed.branches.len(), 2);
        assert_eq!(parsed.branches[0].start_line.to_string(), "-1");
        assert_eq!(parsed.branches[0].target_line.unwrap().to_string(), "8");
        assert_eq!(parsed.branches[1].start_line.to_string(), "8");
        assert_eq!(parsed.branches[1].target_line.unwrap().to_string(), "-1");
    }

    #[test]
    fn coverage_py_rejects_malformed_lines_arcs_contexts_and_metadata() {
        let mut reports = Vec::new();
        for line in [json!(0), json!(-1), json!(1.5), json!("1")] {
            let mut report = coverage_py_report();
            report["files"] = json!({"a.py": {
                "executed_lines":[line], "missing_lines":[]
            }});
            reports.push(report);
        }
        for arc in [json!([1]), json!([1, 0]), json!(["1", 2])] {
            let mut report = coverage_py_report();
            report["files"] = json!({"a.py": {
                "executed_lines":[1], "missing_lines":[], "executed_branches":[arc]
            }});
            reports.push(report);
        }
        for contexts in [
            json!({"0":["test"]}),
            json!({"1":"test"}),
            json!({"1":["bad\ncontext"]}),
        ] {
            let mut report = coverage_py_report();
            report["files"] = json!({"a.py": {
                "executed_lines":[1], "missing_lines":[], "contexts":contexts
            }});
            reports.push(report);
        }
        let mut bad_version = coverage_py_report();
        bad_version["meta"]["version"] = "x".repeat(201).into();
        reports.push(bad_version);

        for report in reports {
            assert!(
                parse_report(
                    CoverageFormat::CoveragePy,
                    report.to_string().as_bytes(),
                    Path::new("/repo")
                )
                .is_err(),
                "accepted malformed report: {report}"
            );
        }
    }

    #[test]
    fn coverage_py_rejects_duplicate_and_contradictory_observation_identities() {
        let entries = [
            json!({
                "executed_lines": [2, 2],
                "missing_lines": []
            }),
            json!({
                "executed_lines": [2],
                "missing_lines": [2]
            }),
            json!({
                "executed_lines": [2],
                "missing_lines": [],
                "contexts": {"2": ["named", "named"]}
            }),
            json!({
                "executed_lines": [2],
                "missing_lines": [],
                "contexts": {"2": ["", ""]}
            }),
            json!({
                "executed_lines": [1],
                "missing_lines": [],
                "executed_branches": [[2, 3], [2, 3]],
                "missing_branches": []
            }),
            json!({
                "executed_lines": [1],
                "missing_lines": [],
                "executed_branches": [[2, 3]],
                "missing_branches": [[2, 3]]
            }),
        ];
        for entry in entries {
            let report = json!({
                "meta": {"format": 3, "version": "7.0"},
                "files": {"pkg/a.py": entry}
            });
            assert!(
                parse_report(
                    CoverageFormat::CoveragePy,
                    report.to_string().as_bytes(),
                    Path::new("/repo")
                )
                .is_err(),
                "accepted duplicate or contradictory observations: {report}"
            );
        }
    }

    #[test]
    fn coverage_py_rejects_wrong_format_and_summary_only_files() {
        for report in [
            json!({"meta":{"format":2,"version":"7.0"},"files":{}}),
            json!({
                "meta":{"format":3,"version":"7.0"},
                "files":{"pkg/a.py":{"summary":{}}},
                "totals":{}
            }),
        ] {
            assert!(
                parse_report(
                    CoverageFormat::CoveragePy,
                    report.to_string().as_bytes(),
                    Path::new("/repo")
                )
                .is_err()
            );
        }
    }
}
