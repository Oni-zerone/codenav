//! Supported languages: file detection, tree-sitter grammars and extraction queries.
//!
//! Every query follows the same capture convention, interpreted by `extract.rs`:
//! - `@def.<kind>` + `@name`: a definition (kind: class, struct, enum, interface,
//!   function, method, type, module, constant, macro); optional `@owner` gives an
//!   explicit owner type (Go method receivers).
//! - `@impl` + `@impl.type`: a block adding members to a type (Rust `impl`, Swift
//!   `extension`); optional `@impl.trait` is the implemented trait/protocol.
//! - `@call` + `@name`: a call site; optional `@qualifier` (receiver expression) or
//!   `@scope` (static path: `Type::f`, `module::f`).
//! - `@import` + `@path`: an import; optional `@import.name` (imported member).
//! - `@import.mod` + `@path`: Rust `mod foo;` declaration.
//! - `@base`: a supertype in an inheritance/conformance clause.
//! - `@typeref`: a reference to a type name.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tree_sitter::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Swift,
}

impl Lang {
    pub fn from_path(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?;
        Some(match ext {
            "rs" => Lang::Rust,
            "py" | "pyi" => Lang::Python,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "go" => Lang::Go,
            "swift" => Lang::Swift,
            _ => return None,
        })
    }

    pub fn grammar(self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Swift => tree_sitter_swift::LANGUAGE.into(),
        }
    }

    pub fn query_source(self) -> String {
        match self {
            Lang::Rust => RUST_QUERY.to_string(),
            Lang::Python => PYTHON_QUERY.to_string(),
            Lang::JavaScript => format!("{JS_COMMON_QUERY}\n{JS_ONLY_QUERY}\n{JSX_QUERY}"),
            Lang::TypeScript => format!("{JS_COMMON_QUERY}\n{TS_QUERY}"),
            Lang::Tsx => format!("{JS_COMMON_QUERY}\n{TS_QUERY}\n{JSX_QUERY}"),
            Lang::Go => GO_QUERY.to_string(),
            Lang::Swift => SWIFT_QUERY.to_string(),
        }
    }

    /// Languages whose symbols can reference each other.
    pub fn family(self) -> &'static str {
        match self {
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => "js",
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::Swift => "swift",
        }
    }

    pub const ALL: [Lang; 7] = [
        Lang::Rust,
        Lang::Python,
        Lang::JavaScript,
        Lang::TypeScript,
        Lang::Tsx,
        Lang::Go,
        Lang::Swift,
    ];
}

const RUST_QUERY: &str = r#"
(struct_item name: (type_identifier) @name) @def.struct
(union_item name: (type_identifier) @name) @def.struct
(enum_item name: (type_identifier) @name) @def.enum
(type_item name: (type_identifier) @name) @def.type
(trait_item name: (type_identifier) @name) @def.interface
(function_item name: (identifier) @name) @def.function
(function_signature_item name: (identifier) @name) @def.function
(mod_item name: (identifier) @name) @def.module
(macro_definition name: (identifier) @name) @def.macro
(const_item name: (identifier) @name) @def.constant
(static_item name: (identifier) @name) @def.constant

(impl_item trait: (_) @impl.trait type: (_) @impl.type) @impl
(impl_item !trait type: (_) @impl.type) @impl

(call_expression function: (identifier) @name) @call
(call_expression function: (field_expression value: (_) @qualifier field: (field_identifier) @name)) @call
(call_expression function: (scoped_identifier path: (_) @scope name: (identifier) @name)) @call
(call_expression function: (generic_function function: (identifier) @name)) @call
(call_expression function: (generic_function function: (scoped_identifier path: (_) @scope name: (identifier) @name))) @call
(macro_invocation macro: (identifier) @name) @call
(struct_expression name: (type_identifier) @name) @call

(use_declaration argument: (_) @path) @import
(mod_item !body name: (identifier) @path) @import.mod

(trait_item bounds: (trait_bounds (type_identifier) @base))

(type_identifier) @typeref
"#;

const PYTHON_QUERY: &str = r#"
(class_definition name: (identifier) @name) @def.class
(function_definition name: (identifier) @name) @def.function

(call function: (identifier) @name) @call
(call function: (attribute object: (_) @qualifier attribute: (identifier) @name)) @call

(import_statement name: (dotted_name) @path) @import
(import_statement name: (aliased_import name: (dotted_name) @path)) @import
(import_from_statement module_name: (_) @path name: (dotted_name) @import.name) @import
(import_from_statement module_name: (_) @path name: (aliased_import name: (dotted_name) @import.name)) @import
(import_from_statement module_name: (_) @path (wildcard_import)) @import

(class_definition superclasses: (argument_list (identifier) @base))
(class_definition superclasses: (argument_list (attribute attribute: (identifier) @base)))

(type (identifier) @typeref)
(type (generic_type (identifier) @typeref))
"#;

/// Shared by JavaScript, TypeScript and TSX.
const JS_COMMON_QUERY: &str = r#"
(class_declaration name: (_) @name) @def.class
(function_declaration name: (identifier) @name) @def.function
(generator_function_declaration name: (identifier) @name) @def.function
(method_definition name: (_) @name) @def.method
(variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @def.function

(call_expression function: (identifier) @name) @call
(call_expression function: (member_expression object: (_) @qualifier property: (property_identifier) @name)) @call
(new_expression constructor: (identifier) @name) @call
(new_expression constructor: (member_expression object: (_) @qualifier property: (property_identifier) @name)) @call

; Top-level `const` (functions are matched above and win over constants).
(program (lexical_declaration kind: "const" (variable_declarator name: (identifier) @name) @def.constant))
(program (export_statement declaration: (lexical_declaration kind: "const" (variable_declarator name: (identifier) @name) @def.constant)))

; Exports: any export statement makes the file an ES module; named/default exports.
(export_statement) @export
(export_statement (export_clause (export_specifier name: (identifier) @export.name)))
(export_statement value: (identifier) @export.name)

; Values that may name a top-level constant.
(identifier) @valueref
(shorthand_property_identifier) @valueref

(import_statement source: (string (string_fragment) @path)) @import
(export_statement source: (string (string_fragment) @path)) @import.reexport
((call_expression function: (identifier) @_req arguments: (arguments (string (string_fragment) @path)))
  (#eq? @_req "require")) @import
((call_expression function: (import) arguments: (arguments (string (string_fragment) @path)))) @import
"#;

const JS_ONLY_QUERY: &str = r#"
(class_heritage (identifier) @base)
(class_heritage (member_expression property: (property_identifier) @base))
"#;

const TS_QUERY: &str = r#"
(abstract_class_declaration name: (type_identifier) @name) @def.class
(interface_declaration name: (type_identifier) @name) @def.interface
(type_alias_declaration name: (type_identifier) @name) @def.type
(enum_declaration name: (identifier) @name) @def.enum
(function_signature name: (identifier) @name) @def.function
(method_signature name: (_) @name) @def.method
(abstract_method_signature name: (_) @name) @def.method
(internal_module name: (_) @name) @def.module

(extends_clause value: (identifier) @base)
(extends_clause value: (member_expression property: (property_identifier) @base))
(implements_clause (type_identifier) @base)
(implements_clause (generic_type name: (type_identifier) @base))
(extends_type_clause type: (type_identifier) @base)
(extends_type_clause type: (generic_type name: (type_identifier) @base))

(type_identifier) @typeref
"#;

/// Components only: lowercase tags (`<p>`, `<summary>`) are HTML elements, not calls.
const JSX_QUERY: &str = r#"
((jsx_opening_element name: (identifier) @name) @call (#match? @name "^[A-Z]"))
((jsx_self_closing_element name: (identifier) @name) @call (#match? @name "^[A-Z]"))
(jsx_opening_element name: (member_expression object: (_) @qualifier property: (property_identifier) @name)) @call
(jsx_self_closing_element name: (member_expression object: (_) @qualifier property: (property_identifier) @name)) @call
"#;

const GO_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @def.function
(method_declaration
  receiver: (parameter_list (parameter_declaration type: (_) @owner))
  name: (field_identifier) @name) @def.method
(type_spec name: (type_identifier) @name type: (struct_type)) @def.struct
(type_spec name: (type_identifier) @name type: (interface_type)) @def.interface
(type_spec name: (type_identifier) @name) @def.type
(const_spec name: (identifier) @name) @def.constant

(call_expression function: (identifier) @name) @call
(call_expression function: (selector_expression operand: (_) @qualifier field: (field_identifier) @name)) @call
(composite_literal type: (type_identifier) @name) @call

(import_spec path: (interpreted_string_literal) @path) @import

(struct_type (field_declaration_list (field_declaration !name type: (type_identifier) @base)))
(interface_type (type_elem (type_identifier) @base))

(type_identifier) @typeref
"#;

const SWIFT_QUERY: &str = r#"
(class_declaration declaration_kind: "class" name: (type_identifier) @name) @def.class
(class_declaration declaration_kind: "actor" name: (type_identifier) @name) @def.class
(class_declaration declaration_kind: "struct" name: (type_identifier) @name) @def.struct
(class_declaration declaration_kind: "enum" name: (type_identifier) @name) @def.enum
(class_declaration declaration_kind: "extension" name: (_) @impl.type) @impl
(protocol_declaration name: (type_identifier) @name) @def.interface
(function_declaration name: (simple_identifier) @name) @def.function
(protocol_function_declaration name: (simple_identifier) @name) @def.method
(init_declaration "init" @name) @def.method
(typealias_declaration name: (type_identifier) @name) @def.type

(call_expression (simple_identifier) @name) @call
(call_expression (navigation_expression target: (_) @qualifier suffix: (navigation_suffix suffix: (simple_identifier) @name))) @call

(import_declaration (identifier) @path) @import

(inheritance_specifier inherits_from: (user_type (type_identifier) @base))

(user_type (type_identifier) @typeref)
"#;
