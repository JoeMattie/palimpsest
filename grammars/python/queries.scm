; imports
(import_statement name: (dotted_name) @spec)
(import_statement name: (aliased_import name: (dotted_name) @spec))
(import_from_statement module_name: (dotted_name) @spec.from)
(import_from_statement module_name: (relative_import) @spec.from)
(import_from_statement name: (dotted_name) @import.name)
(import_from_statement name: (aliased_import name: (dotted_name) @import.name))

; defs
(function_definition name: (identifier) @def.fn)
(class_definition name: (identifier) @def.class)

; calls
(call function: (identifier) @call)
(call function: (attribute attribute: (identifier) @call))
