(namespace_definition) @namespace

[
  (class_specifier)
  (struct_specifier)
  (union_specifier)
  (enum_specifier)
] @type

(function_definition) @function
(preproc_include) @include
(call_expression) @call
(field_declaration declarator: (_) @member)

[
  (init_declarator declarator: (_) @binding)
  (parameter_declaration declarator: (_) @binding)
  (optional_parameter_declaration declarator: (_) @binding)
  (declaration declarator: (identifier) @binding)
  (for_range_loop declarator: (_) @binding)
]

(structured_binding_declarator (identifier) @binding)
