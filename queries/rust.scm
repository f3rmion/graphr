[
  (struct_item)
  (enum_item)
  (union_item)
  (trait_item)
  (type_item)
  (associated_type)
] @type

(mod_item
  body: (declaration_list)) @module

(impl_item) @implementation
[
  (function_item)
  (function_signature_item)
] @function
(attribute_item) @attribute
(use_declaration) @import
[
  (parameter) @binding
  (let_declaration) @binding
  (let_condition pattern: (_) @binding)
  (for_expression pattern: (_) @binding)
  (match_arm pattern: (_) @binding)
  (closure_parameters (_) @binding)
  (const_item name: (identifier) @binding)
  (static_item name: (identifier) @binding)
]

(call_expression) @call
(macro_invocation) @macro
