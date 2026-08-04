(class_definition) @type
(function_definition) @function
[
  (import_statement)
  (import_from_statement)
] @import
(function_definition
  parameters: (parameters) @binding)
[
  (assignment left: (_) @binding)
  (for_statement left: (_) @binding)
]
(call
  function: (identifier) @call)
