use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde::Deserialize;

use crate::git::capture_evidence_file;
use crate::workspace::{ErrorCode, OperationError};

pub const MANIFEST_LIMIT: u64 = 64 * 1024;
pub const ARTIFACT_LIMIT: u64 = 2 * 1024 * 1024;
#[allow(dead_code)] // Task 3 consumes this already-fixed trust-boundary limit.
pub const COVERAGE_LIMIT: u64 = 64 * 1024 * 1024;
pub const GENERATED_LIMIT: usize = 64;
pub const COVERAGE_REPORT_LIMIT: usize = 8;
pub const EVIDENCE_TOTAL_LIMIT: u64 = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum CoverageFormat {
    Llvm,
    CoveragePy,
}

impl CoverageFormat {
    pub const fn db(self) -> &'static str {
        match self {
            Self::Llvm => "llvm",
            Self::CoveragePy => "coverage_py",
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SourceSpan {
    pub path: String,
    pub line_start: u32,
    pub line_end: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedArtifact {
    pub path: String,
    pub content_hash: [u8; 32],
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedArtifactSpan {
    pub artifact: CapturedArtifact,
    pub line_start: u32,
    pub line_end: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedEvidence {
    pub source_snapshot_id: String,
    pub manifest: CapturedArtifact,
    pub generated: Vec<CapturedGenerated>,
    pub coverage: Vec<CapturedCoverage>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedGenerated {
    pub input: CapturedArtifactSpan,
    pub generator: SourceSpan,
    pub output: CapturedArtifactSpan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedCoverage {
    pub format: CoverageFormat,
    pub report: CapturedArtifact,
    pub run_label: String,
    pub test_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields)]
struct ManifestArtifactSpan {
    path: String,
    blake3: String,
    line_start: u32,
    line_end: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields)]
struct ManifestSourceSpan {
    path: String,
    line_start: u32,
    line_end: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields)]
struct ManifestGenerated {
    input: ManifestArtifactSpan,
    generator: ManifestSourceSpan,
    output: ManifestArtifactSpan,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields)]
struct ManifestCoverage {
    format: CoverageFormat,
    path: String,
    blake3: String,
    run_label: String,
    test_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format_version: u32,
    source_snapshot_id: String,
    generated: Vec<ManifestGenerated>,
    coverage: Vec<ManifestCoverage>,
}

pub(crate) struct EvidenceManifest {
    source_snapshot_id: String,
    manifest: CapturedArtifact,
    generated: Vec<ManifestGenerated>,
    coverage: Vec<ManifestCoverage>,
}

impl EvidenceManifest {
    #[cfg(test)]
    pub(crate) fn source_snapshot_id(&self) -> &str {
        &self.source_snapshot_id
    }

    pub(crate) fn requested_artifact_paths(&self) -> BTreeSet<String> {
        self.generated
            .iter()
            .map(|generated| generated.input.path.clone())
            .collect()
    }

    pub(crate) fn evidence_only_paths(&self) -> BTreeSet<String> {
        std::iter::once(self.manifest.path.clone())
            .chain(
                self.generated
                    .iter()
                    .map(|generated| generated.output.path.clone()),
            )
            .chain(self.coverage.iter().map(|coverage| coverage.path.clone()))
            .collect()
    }

    pub(crate) fn capture(
        self,
        root: &Path,
        inputs: &BTreeMap<String, CapturedArtifact>,
        cancelled: &AtomicBool,
    ) -> Result<CapturedEvidence, OperationError> {
        let mut unique = BTreeMap::<String, ([u8; 32], u64)>::new();
        record_unique(&mut unique, &self.manifest)?;
        let mut generated = Vec::with_capacity(self.generated.len());
        for declaration in self.generated {
            let expected_input = parse_digest(&declaration.input.blake3)?;
            let input = inputs.get(&declaration.input.path).ok_or_else(|| {
                invalid("declared input artifact is absent from the selected source state")
            })?;
            if input.path != declaration.input.path || input.content_hash != expected_input {
                return Err(invalid("declared input artifact digest does not match"));
            }
            if input.bytes.len() as u64 > ARTIFACT_LIMIT {
                return Err(invalid("input artifact exceeds its size limit"));
            }
            validate_span(
                &input.bytes,
                declaration.input.line_start,
                declaration.input.line_end,
            )?;
            record_unique(&mut unique, input)?;

            let bytes =
                capture_evidence_file(root, &declaration.output.path, ARTIFACT_LIMIT, cancelled)?;
            if std::str::from_utf8(&bytes).is_err() {
                return Err(invalid("generated Rust output is not valid UTF-8"));
            }
            let content_hash = *blake3::hash(&bytes).as_bytes();
            if content_hash != parse_digest(&declaration.output.blake3)? {
                return Err(invalid("generated artifact digest does not match"));
            }
            validate_span(
                &bytes,
                declaration.output.line_start,
                declaration.output.line_end,
            )?;
            let output = CapturedArtifact {
                path: declaration.output.path,
                content_hash,
                bytes,
            };
            record_unique(&mut unique, &output)?;
            generated.push(CapturedGenerated {
                input: CapturedArtifactSpan {
                    artifact: input.clone(),
                    line_start: declaration.input.line_start,
                    line_end: declaration.input.line_end,
                },
                generator: SourceSpan {
                    path: declaration.generator.path,
                    line_start: declaration.generator.line_start,
                    line_end: declaration.generator.line_end,
                },
                output: CapturedArtifactSpan {
                    artifact: output,
                    line_start: declaration.output.line_start,
                    line_end: declaration.output.line_end,
                },
            });
        }
        let total = unique
            .values()
            .try_fold(0_u64, |total, (_, size)| total.checked_add(*size))
            .ok_or_else(|| invalid("captured evidence byte total exceeds its limit"))?;
        if total > EVIDENCE_TOTAL_LIMIT {
            return Err(invalid("captured evidence byte total exceeds its limit"));
        }
        Ok(CapturedEvidence {
            source_snapshot_id: self.source_snapshot_id,
            manifest: self.manifest,
            generated,
            coverage: Vec::new(),
        })
    }
}

pub(crate) fn capture_manifest(
    root: &Path,
    manifest_path: &Path,
    cancelled: &AtomicBool,
) -> Result<EvidenceManifest, OperationError> {
    let path = safe_path(manifest_path)?;
    let bytes = capture_evidence_file(root, &path, MANIFEST_LIMIT, cancelled)?;
    let content_hash = *blake3::hash(&bytes).as_bytes();
    let mut value: Manifest = rmcp::serde_json::from_slice(&bytes)
        .map_err(|_| invalid("evidence manifest is invalid"))?;
    if value.format_version != 1 {
        return Err(invalid("evidence manifest format version is unsupported"));
    }
    if !valid_digest(&value.source_snapshot_id) {
        return Err(invalid("source snapshot ID is invalid"));
    }
    if value.generated.len() > GENERATED_LIMIT {
        return Err(invalid("evidence manifest has too many generated mappings"));
    }
    if value.coverage.len() > COVERAGE_REPORT_LIMIT {
        return Err(invalid("evidence manifest has too many coverage reports"));
    }
    for generated in &value.generated {
        validate_artifact_span(&generated.input, false)?;
        validate_source_span(&generated.generator)?;
        validate_artifact_span(&generated.output, true)?;
    }
    for coverage in &value.coverage {
        let _ = safe_str_path(&coverage.path)?;
        let _ = parse_digest(&coverage.blake3)?;
        validate_label(&coverage.run_label)?;
        if let Some(test_name) = &coverage.test_name {
            validate_label(test_name)?;
        }
    }
    value.generated.sort_unstable();
    if value.generated.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid(
            "evidence manifest contains a duplicate generated link",
        ));
    }
    value.coverage.sort_unstable();
    let mut runs = BTreeSet::new();
    if value
        .coverage
        .iter()
        .any(|coverage| !runs.insert((&coverage.run_label, &coverage.test_name)))
    {
        return Err(invalid(
            "evidence manifest contains a duplicate coverage run",
        ));
    }
    if !value.coverage.is_empty() {
        return Err(invalid(
            "coverage evidence is unsupported by this graph format",
        ));
    }
    Ok(EvidenceManifest {
        source_snapshot_id: value.source_snapshot_id,
        manifest: CapturedArtifact {
            path,
            content_hash,
            bytes,
        },
        generated: value.generated,
        coverage: value.coverage,
    })
}

fn validate_artifact_span(
    span: &ManifestArtifactSpan,
    generated: bool,
) -> Result<(), OperationError> {
    let _ = safe_str_path(&span.path)?;
    let _ = parse_digest(&span.blake3)?;
    validate_declared_span(span.line_start, span.line_end)?;
    if generated && !span.path.ends_with(".rs") {
        return Err(invalid("generated artifact must be Rust source"));
    }
    Ok(())
}

fn validate_source_span(span: &ManifestSourceSpan) -> Result<(), OperationError> {
    let _ = safe_str_path(&span.path)?;
    validate_declared_span(span.line_start, span.line_end)
}

fn validate_declared_span(start: u32, end: u32) -> Result<(), OperationError> {
    if start == 0 || end < start {
        Err(invalid("evidence span is invalid"))
    } else {
        Ok(())
    }
}

fn validate_span(bytes: &[u8], start: u32, end: u32) -> Result<(), OperationError> {
    validate_declared_span(start, end)?;
    let lines = bytes.iter().filter(|byte| **byte == b'\n').count()
        + usize::from(bytes.last().is_some_and(|byte| *byte != b'\n'));
    let lines = u32::try_from(lines.max(1)).map_err(|_| invalid("artifact has too many lines"))?;
    if end > lines {
        Err(invalid("evidence span exceeds captured artifact lines"))
    } else {
        Ok(())
    }
}

fn validate_label(value: &str) -> Result<(), OperationError> {
    if value.is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
        Err(invalid("evidence label is invalid"))
    } else {
        Ok(())
    }
}

fn parse_digest(value: &str) -> Result<[u8; 32], OperationError> {
    if !valid_digest(value) {
        return Err(invalid("evidence digest is invalid"));
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = (hex(pair[0]) << 4) | hex(pair[1]);
    }
    Ok(digest)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn hex(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("validated hexadecimal digit"),
    }
}

fn safe_path(path: &Path) -> Result<String, OperationError> {
    let value = path
        .to_str()
        .ok_or_else(|| invalid("evidence path is not valid UTF-8"))?;
    safe_str_path(value)
}

fn safe_str_path(value: &str) -> Result<String, OperationError> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 1024
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || path.is_absolute()
    {
        Err(invalid("evidence path is unsafe"))
    } else {
        Ok(value.to_owned())
    }
}

fn record_unique(
    unique: &mut BTreeMap<String, ([u8; 32], u64)>,
    artifact: &CapturedArtifact,
) -> Result<(), OperationError> {
    let size = u64::try_from(artifact.bytes.len())
        .map_err(|_| invalid("captured artifact size exceeds supported range"))?;
    match unique.entry(artifact.path.clone()) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert((artifact.content_hash, size));
            Ok(())
        }
        std::collections::btree_map::Entry::Occupied(entry)
            if *entry.get() == (artifact.content_hash, size) =>
        {
            Ok(())
        }
        std::collections::btree_map::Entry::Occupied(_) => {
            Err(invalid("evidence artifact identity conflicts"))
        }
    }
}

fn invalid(message: &'static str) -> OperationError {
    OperationError::new(ErrorCode::InvalidParameters, message)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::git::set_after_evidence_read_hook;
    use std::path::PathBuf;

    #[test]
    fn manifest_validation_accepts_exact_bytes_and_spans() {
        let root = fixture("valid");
        let input = b"syntax = one\nmessage = two\n".to_vec();
        let output = b"pub fn generated() {}\n".to_vec();
        fs::write(root.join("schema.proto"), &input).unwrap();
        fs::write(root.join("out.rs"), &output).unwrap();
        write_manifest(
            &root,
            &manifest_json(&input, &output, "schema.proto", "out.rs"),
        );

        let manifest =
            capture_manifest(&root, Path::new("evidence.json"), &AtomicBool::new(false)).unwrap();
        assert_eq!(manifest.source_snapshot_id(), "a".repeat(64));
        assert_eq!(
            manifest.requested_artifact_paths(),
            BTreeSet::from(["schema.proto".to_owned()])
        );
        assert_eq!(
            manifest.evidence_only_paths(),
            BTreeSet::from(["evidence.json".to_owned(), "out.rs".to_owned()])
        );
        let evidence = manifest
            .capture(
                &root,
                &BTreeMap::from([(
                    "schema.proto".to_owned(),
                    CapturedArtifact {
                        path: "schema.proto".into(),
                        content_hash: *blake3::hash(&input).as_bytes(),
                        bytes: input,
                    },
                )]),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(evidence.generated.len(), 1);
        assert_eq!(evidence.generated[0].input.line_start, 2);
        assert_eq!(evidence.generated[0].output.artifact.bytes, output);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manifest_validation_rejects_unknown_version_coverage_and_unsafe_paths() {
        let root = fixture("invalid-manifest");
        for value in [
            format!(
                "{{\"format_version\":2,\"source_snapshot_id\":\"{}\",\"generated\":[],\"coverage\":[]}}",
                "a".repeat(64)
            ),
            format!(
                "{{\"format_version\":1,\"source_snapshot_id\":\"{}\",\"generated\":[],\"coverage\":[],\"unknown\":true}}",
                "a".repeat(64)
            ),
            format!(
                "{{\"format_version\":1,\"source_snapshot_id\":\"{}\",\"generated\":[],\"coverage\":[{{\"format\":\"llvm\",\"path\":\"report.json\",\"blake3\":\"{}\",\"run_label\":\"run\"}}]}}",
                "a".repeat(64),
                "b".repeat(64)
            ),
        ] {
            write_manifest(&root, &value);
            assert!(
                capture_manifest(&root, Path::new("evidence.json"), &AtomicBool::new(false))
                    .is_err()
            );
        }
        for path in [
            "../evidence.json",
            "/tmp/evidence.json",
            "bad//evidence.json",
        ] {
            assert!(capture_manifest(&root, Path::new(path), &AtomicBool::new(false)).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manifest_validation_enforces_the_inclusive_manifest_bound() {
        let root = fixture("manifest-bound");
        let base = format!(
            "{{\"format_version\":1,\"source_snapshot_id\":\"{}\",\"generated\":[],\"coverage\":[]}}",
            "a".repeat(64)
        );
        let mut exact = base.into_bytes();
        exact.resize(MANIFEST_LIMIT as usize, b' ');
        fs::write(root.join("evidence.json"), &exact).unwrap();
        capture_manifest(&root, Path::new("evidence.json"), &AtomicBool::new(false)).unwrap();
        exact.push(b' ');
        fs::write(root.join("evidence.json"), exact).unwrap();
        assert!(
            capture_manifest(&root, Path::new("evidence.json"), &AtomicBool::new(false)).is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manifest_validation_rejects_non_regular_or_missing_manifest() {
        let root = fixture("manifest-file-kind");
        assert!(
            capture_manifest(&root, Path::new("missing.json"), &AtomicBool::new(false)).is_err()
        );
        fs::create_dir(root.join("directory.json")).unwrap();
        assert!(
            capture_manifest(&root, Path::new("directory.json"), &AtomicBool::new(false)).is_err()
        );
        symlink("target.json", root.join("link.json")).unwrap();
        assert!(capture_manifest(&root, Path::new("link.json"), &AtomicBool::new(false)).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manifest_validation_rejects_invalid_digests_spans_labels_and_duplicates() {
        let root = fixture("invalid-fields");
        let input = b"one\ntwo\n".to_vec();
        let output = b"fn generated() {}\n".to_vec();
        fs::write(root.join("schema.proto"), &input).unwrap();
        fs::write(root.join("out.rs"), &output).unwrap();
        let valid: rmcp::serde_json::Value =
            rmcp::serde_json::from_str(&manifest_json(&input, &output, "schema.proto", "out.rs"))
                .unwrap();
        for (index, mut value) in [valid.clone(), valid.clone()].into_iter().enumerate() {
            if index == 0 {
                value["generated"][0]["input"]["blake3"] = "g".repeat(64).into();
            } else {
                value["generated"][0]["input"]["line_start"] = 0.into();
            }
            write_manifest(&root, &value.to_string());
            assert!(
                capture_manifest(&root, Path::new("evidence.json"), &AtomicBool::new(false))
                    .is_err()
            );
        }
        let mut duplicate = valid.clone();
        duplicate["generated"] = rmcp::serde_json::Value::Array(vec![
            valid["generated"][0].clone(),
            valid["generated"][0].clone(),
        ]);
        write_manifest(&root, &duplicate.to_string());
        assert!(
            capture_manifest(&root, Path::new("evidence.json"), &AtomicBool::new(false)).is_err()
        );
        let invalid_coverage = format!(
            "{{\"format_version\":1,\"source_snapshot_id\":\"{}\",\"generated\":[],\"coverage\":[{{\"format\":\"llvm\",\"path\":\"report.json\",\"blake3\":\"{}\",\"run_label\":\"bad\\nlabel\"}}]}}",
            "a".repeat(64),
            "b".repeat(64),
        );
        write_manifest(&root, &invalid_coverage);
        assert!(
            capture_manifest(&root, Path::new("evidence.json"), &AtomicBool::new(false)).is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_artifact_size_limit_is_inclusive() {
        let root = fixture("artifact-bound");
        let input = b"one\ntwo\n".to_vec();
        let mut output = vec![b'x'; ARTIFACT_LIMIT as usize];
        fs::write(root.join("schema.proto"), &input).unwrap();
        fs::write(root.join("out.rs"), &output).unwrap();
        write_manifest(
            &root,
            &manifest_json(&input, &output, "schema.proto", "out.rs"),
        );
        let manifest =
            capture_manifest(&root, Path::new("evidence.json"), &AtomicBool::new(false)).unwrap();
        let inputs = BTreeMap::from([(
            "schema.proto".to_owned(),
            CapturedArtifact {
                path: "schema.proto".into(),
                content_hash: *blake3::hash(&input).as_bytes(),
                bytes: input.clone(),
            },
        )]);
        manifest
            .capture(&root, &inputs, &AtomicBool::new(false))
            .unwrap();

        output.push(b'x');
        fs::write(root.join("out.rs"), &output).unwrap();
        write_manifest(
            &root,
            &manifest_json(&input, &output, "schema.proto", "out.rs"),
        );
        let manifest =
            capture_manifest(&root, Path::new("evidence.json"), &AtomicBool::new(false)).unwrap();
        assert!(
            manifest
                .capture(&root, &inputs, &AtomicBool::new(false))
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manifest_validation_enforces_entry_and_aggregate_bounds() {
        let root = fixture("aggregate-bound");
        let input = b"x\n".to_vec();
        fs::write(root.join("schema.proto"), &input).unwrap();
        let full_output = vec![b'x'; ARTIFACT_LIMIT as usize];
        let mut generated = (0..=GENERATED_LIMIT)
            .map(|index| {
                rmcp::serde_json::json!({
                    "input": {
                        "path": "schema.proto",
                        "blake3": blake3::hash(&input).to_hex().to_string(),
                        "line_start": 1,
                        "line_end": 1
                    },
                    "generator": {
                        "path": "src/generator.rs",
                        "line_start": 1,
                        "line_end": 1
                    },
                    "output": {
                        "path": format!("target/out-{index}.rs"),
                        "blake3": blake3::hash(&full_output).to_hex().to_string(),
                        "line_start": 1,
                        "line_end": 1
                    }
                })
            })
            .collect::<Vec<_>>();
        let manifest_value = |generated: Vec<rmcp::serde_json::Value>| {
            rmcp::serde_json::json!({
                "format_version": 1,
                "source_snapshot_id": "a".repeat(64),
                "generated": generated,
                "coverage": []
            })
        };
        write_manifest(&root, &manifest_value(generated.clone()).to_string());
        assert!(
            capture_manifest(&root, Path::new("evidence.json"), &AtomicBool::new(false)).is_err()
        );

        generated.pop();
        fs::create_dir(root.join("target")).unwrap();
        let provisional = manifest_value(generated.clone()).to_string();
        let final_output_size = ARTIFACT_LIMIT
            - u64::try_from(provisional.len()).unwrap()
            - u64::try_from(input.len()).unwrap();
        let final_output = vec![b'x'; final_output_size as usize];
        generated[GENERATED_LIMIT - 1]["output"]["blake3"] =
            blake3::hash(&final_output).to_hex().to_string().into();
        let exact_manifest = manifest_value(generated.clone()).to_string();
        assert_eq!(exact_manifest.len(), provisional.len());
        for index in 0..GENERATED_LIMIT - 1 {
            fs::write(root.join(format!("target/out-{index}.rs")), &full_output).unwrap();
        }
        fs::write(
            root.join(format!("target/out-{}.rs", GENERATED_LIMIT - 1)),
            &final_output,
        )
        .unwrap();
        write_manifest(&root, &exact_manifest);
        let inputs = BTreeMap::from([(
            "schema.proto".to_owned(),
            CapturedArtifact {
                path: "schema.proto".into(),
                content_hash: *blake3::hash(&input).as_bytes(),
                bytes: input,
            },
        )]);
        let manifest =
            capture_manifest(&root, Path::new("evidence.json"), &AtomicBool::new(false)).unwrap();
        let evidence = manifest
            .capture(&root, &inputs, &AtomicBool::new(false))
            .unwrap();
        drop(evidence);

        let mut over = final_output;
        over.push(b'x');
        generated[GENERATED_LIMIT - 1]["output"]["blake3"] =
            blake3::hash(&over).to_hex().to_string().into();
        fs::write(
            root.join(format!("target/out-{}.rs", GENERATED_LIMIT - 1)),
            &over,
        )
        .unwrap();
        write_manifest(&root, &manifest_value(generated).to_string());
        let manifest =
            capture_manifest(&root, Path::new("evidence.json"), &AtomicBool::new(false)).unwrap();
        assert!(
            manifest
                .capture(&root, &inputs, &AtomicBool::new(false))
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn artifact_replacement_during_read_is_fatal() {
        let root = fixture("replacement");
        let input = b"one\ntwo\n".to_vec();
        let output = b"fn generated() {}\n".to_vec();
        fs::write(root.join("schema.proto"), &input).unwrap();
        fs::write(root.join("out.rs"), &output).unwrap();
        write_manifest(
            &root,
            &manifest_json(&input, &output, "schema.proto", "out.rs"),
        );
        let manifest =
            capture_manifest(&root, Path::new("evidence.json"), &AtomicBool::new(false)).unwrap();
        let replacement_root = root.clone();
        set_after_evidence_read_hook(move || {
            fs::rename(
                replacement_root.join("out.rs"),
                replacement_root.join("old.rs"),
            )
            .unwrap();
            fs::write(replacement_root.join("out.rs"), "fn replacement() {}\n").unwrap();
        });
        let inputs = BTreeMap::from([(
            "schema.proto".to_owned(),
            CapturedArtifact {
                path: "schema.proto".into(),
                content_hash: *blake3::hash(&input).as_bytes(),
                bytes: input,
            },
        )]);
        assert!(
            manifest
                .capture(&root, &inputs, &AtomicBool::new(false))
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn fixture(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "graphr-evidence-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::canonicalize(root).unwrap()
    }

    fn write_manifest(root: &Path, value: &str) {
        fs::write(root.join("evidence.json"), value).unwrap();
    }

    fn manifest_json(input: &[u8], output: &[u8], input_path: &str, output_path: &str) -> String {
        format!(
            "{{\"format_version\":1,\"source_snapshot_id\":\"{}\",\"generated\":[{{\"input\":{{\"path\":\"{input_path}\",\"blake3\":\"{}\",\"line_start\":2,\"line_end\":2}},\"generator\":{{\"path\":\"src/generator.rs\",\"line_start\":1,\"line_end\":1}},\"output\":{{\"path\":\"{output_path}\",\"blake3\":\"{}\",\"line_start\":1,\"line_end\":1}}}}],\"coverage\":[]}}",
            "a".repeat(64),
            blake3::hash(input).to_hex(),
            blake3::hash(output).to_hex(),
        )
    }
}
