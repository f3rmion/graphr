use rmcp::schemars::{Schema, SchemaGenerator};

/// JSON Schema has no unsigned integer format, and the `uint*` formats
/// emitted by schemars' built-in unsigned impls are not understood by strict
/// validators. Fields override with a plain non-negative integer schema.
pub fn unsigned_integer(_: &mut SchemaGenerator) -> Schema {
    rmcp::schemars::json_schema!({ "type": "integer", "minimum": 0 })
}
