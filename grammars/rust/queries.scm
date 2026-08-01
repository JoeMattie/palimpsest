; imports: whole use declarations, expanded textually by the extractor
(use_declaration) @use_decl
(mod_item name: (identifier) @mod !body)

; defs
(function_item name: (identifier) @def.fn)
(struct_item name: (type_identifier) @def.struct)
(enum_item name: (type_identifier) @def.enum)
(trait_item name: (type_identifier) @def.trait)
(type_item name: (type_identifier) @def.type)
(macro_definition name: (identifier) @def.macro)
(static_item name: (identifier) @def.static)
(const_item name: (identifier) @def.const)

; calls
(call_expression function: (identifier) @call)
(call_expression function: (scoped_identifier name: (identifier) @call))
(call_expression function: (field_expression field: (field_identifier) @call))
(macro_invocation macro: (identifier) @call)

; type references
(type_identifier) @typeref
