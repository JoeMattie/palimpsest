; imports
(import_statement source: (string) @spec)
(export_statement source: (string) @spec.reexport)
(call_expression
  function: (identifier) @_req
  arguments: (arguments (string) @spec)
  (#eq? @_req "require"))
(import_statement
  (import_clause (named_imports (import_specifier name: (identifier) @import.name))))
(import_statement (import_clause (identifier) @import.name))

; defs
(function_declaration name: (identifier) @def.fn)
(generator_function_declaration name: (identifier) @def.fn)
(class_declaration name: (identifier) @def.class)
(variable_declarator name: (identifier) @def.const)
(method_definition name: (property_identifier) @def.method)

; calls
(call_expression function: (identifier) @call)
(call_expression function: (member_expression property: (property_identifier) @call))
(new_expression constructor: (identifier) @call)
