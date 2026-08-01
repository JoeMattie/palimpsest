; imports
(import_spec path: (interpreted_string_literal) @spec)
(import_spec path: (raw_string_literal) @spec)

; defs
(function_declaration name: (identifier) @def.fn)
(method_declaration name: (field_identifier) @def.method)
(type_declaration (type_spec name: (type_identifier) @def.type))
(const_declaration (const_spec name: (identifier) @def.const))
(var_declaration (var_spec name: (identifier) @def.var))

; calls
(call_expression function: (identifier) @call)
(call_expression function: (selector_expression field: (field_identifier) @call))

; type references
(type_identifier) @typeref
