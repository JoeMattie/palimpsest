; requires. require_relative resolves against the file's own directory, so
; it is captured separately and prefixed rel: by the extractor.
(call
  method: (identifier) @_req
  arguments: (argument_list (string (string_content) @spec))
  (#eq? @_req "require"))
(call
  method: (identifier) @_req
  arguments: (argument_list (string (string_content) @spec.relative))
  (#eq? @_req "require_relative"))

; defs
(class name: (constant) @def.class)
(module name: (constant) @def.module)
(method name: (identifier) @def.fn)
(singleton_method name: (identifier) @def.fn)
(assignment left: (constant) @def.const)

; calls
(call method: (identifier) @call)

; type references
(constant) @typeref
