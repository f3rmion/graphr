[
  (class_declaration)
  (class)
  (function_declaration)
  (generator_function_declaration)
  (function_expression)
  (generator_function)
  (arrow_function)
  (method_definition)
] @definition

[
  (import_statement)
  (export_statement)
  (assignment_expression)
] @module

[
  (call_expression)
  (new_expression)
] @call

[
  (formal_parameters)
  (variable_declarator)
  (catch_clause)
] @binding

(arrow_function
  parameter: (identifier)) @binding
