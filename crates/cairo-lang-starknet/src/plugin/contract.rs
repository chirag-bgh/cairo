use std::vec;

use cairo_lang_defs::db::get_all_path_leafs;
use cairo_lang_defs::plugin::{
    DynGeneratedFileAuxData, PluginDiagnostic, PluginGeneratedFile, PluginResult,
};
use cairo_lang_semantic::patcher::{PatchBuilder, RewriteNode};
use cairo_lang_semantic::plugin::DynPluginAuxData;
use cairo_lang_syntax::node::ast::{MaybeModuleBody, OptionWrappedGenericParamList};
use cairo_lang_syntax::node::db::SyntaxGroup;
use cairo_lang_syntax::node::helpers::{GetIdentifier, QueryAttrs};
use cairo_lang_syntax::node::kind::SyntaxKind;
use cairo_lang_syntax::node::{ast, Terminal, TypedSyntaxNode};
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;
use indoc::formatdoc;

use super::consts::{
    ABI_TRAIT, CONSTRUCTOR_MODULE, CONTRACT_ATTR, EVENT_ATTR, EXTERNAL_ATTR, EXTERNAL_MODULE,
    L1_HANDLER_FIRST_PARAM_NAME, L1_HANDLER_MODULE, STORAGE_STRUCT_NAME,
};
use super::entry_point::{generate_entry_point_wrapper, EntryPointKind};
use super::events::handle_event;
use super::storage::handle_storage_struct;
use super::utils::{is_felt252, is_mut_param, maybe_strip_underscore};
use crate::contract::starknet_keccak;
use crate::plugin::aux_data::StarkNetContractAuxData;

/// Handles a contract module item.
pub fn handle_module(db: &dyn SyntaxGroup, module_ast: ast::ItemModule) -> PluginResult {
    if !module_ast.has_attr(db, "contract") {
        return PluginResult::default();
    }
    let MaybeModuleBody::Some(body) = module_ast.body(db) else {
        return PluginResult {
            code: None,
            diagnostics: vec![PluginDiagnostic {
                message: "Contracts without body are not supported.".to_string(),
                stable_ptr: module_ast.stable_ptr().untyped(),
            }],
            remove_original_item: false,
        };
    };
    let Some(storage_struct_ast) = body.items(db).elements(db).into_iter().find(|item| {
        matches!(item, ast::Item::Struct(struct_ast) if struct_ast.name(db).text(db) == "Storage")
    }) else {
        return PluginResult {
            code: None,
            diagnostics: vec![PluginDiagnostic {
                message: "Contracts must define a 'Storage' struct.".to_string(),
                stable_ptr: module_ast.stable_ptr().untyped(),
            }],
            remove_original_item: false,
        };
    };

    if !storage_struct_ast.has_attr(db, "starknet::storage") {
        return PluginResult {
            code: None,
            diagnostics: vec![PluginDiagnostic {
                message: "'Storage' struct must be annotated with #[starknet::storage]."
                    .to_string(),
                stable_ptr: module_ast.stable_ptr().untyped(),
            }],
            remove_original_item: false,
        };
    }

    PluginResult::default()
}

/// Accumulated data for contract generation.
#[derive(Default)]
struct ContractGenerationData {
    generated_external_functions: Vec<RewriteNode>,
    generated_constructor_functions: Vec<RewriteNode>,
    generated_l1_handler_functions: Vec<RewriteNode>,
    abi_functions: Vec<RewriteNode>,
    event_functions: Vec<RewriteNode>,
    abi_events: Vec<RewriteNode>,
}

/// If the module is annotated with CONTRACT_ATTR, generate the relevant contract logic.
pub fn handle_contract_by_storage(
    db: &dyn SyntaxGroup,
    struct_ast: ast::ItemStruct,
) -> Option<PluginResult> {
    let module_node = struct_ast.as_syntax_node().parent()?.parent()?.parent()?;
    if module_node.kind(db) != SyntaxKind::ItemModule {
        return None;
    }
    let module_ast = ast::ItemModule::from_syntax_node(db, module_node);

    if !module_ast.has_attr(db, CONTRACT_ATTR) {
        return None;
    }

    let body = match module_ast.body(db) {
        MaybeModuleBody::Some(body) => body,
        MaybeModuleBody::None(empty_body) => {
            return Some(PluginResult {
                code: None,
                diagnostics: vec![PluginDiagnostic {
                    message: "Contracts without body are not supported.".to_string(),
                    stable_ptr: empty_body.stable_ptr().untyped(),
                }],
                remove_original_item: false,
            });
        }
    };
    let mut diagnostics = vec![];
    let mut kept_original_items = Vec::new();

    // A mapping from a 'use' item to its path.
    let mut extra_uses = OrderedHashMap::default();
    let mut has_event = false;
    for item in body.items(db).elements(db) {
        // Skipping elements that only generate other code, but their code itself is ignored.
        if matches!(&item, ast::Item::FreeFunction(item) if item.has_attr(db, EVENT_ATTR))
            || matches!(&item, ast::Item::Struct(item) if item.name(db).text(db) == STORAGE_STRUCT_NAME)
        {
            continue;
        }
        if matches!(&item, ast::Item::Struct(item) if item.name(db).text(db) == "Event")
            || matches!(&item, ast::Item::Enum(item) if item.name(db).text(db) == "Event")
        {
            has_event = true;
        }
        kept_original_items.push(RewriteNode::Copied(item.as_syntax_node()));
        if let Some(ident) = match item {
            ast::Item::Constant(item) => Some(item.name(db)),
            ast::Item::Module(item) => Some(item.name(db)),
            ast::Item::Use(item) => {
                let leaves = get_all_path_leafs(db, item.use_path(db));
                for leaf in leaves {
                    if leaf.stable_ptr().identifier(db) == "Event" {
                        has_event = true;
                    }
                    extra_uses
                        .entry(leaf.stable_ptr().identifier(db))
                        .or_insert_with_key(|ident| format!("super::{}", ident));
                }
                None
            }
            ast::Item::Impl(item) => Some(item.name(db)),
            ast::Item::Struct(item) => Some(item.name(db)),
            ast::Item::Enum(item) => Some(item.name(db)),
            ast::Item::TypeAlias(item) => Some(item.name(db)),
            // Externs, trait declarations and free functions are not directly required in generated
            // inner modules.
            ast::Item::ExternFunction(_)
            | ast::Item::ExternType(_)
            | ast::Item::Trait(_)
            | ast::Item::FreeFunction(_)
            | ast::Item::Missing(_) => None,
            ast::Item::ImplAlias(_) => todo!(),
        } {
            extra_uses
                .entry(ident.text(db))
                .or_insert_with_key(|ident| format!("super::{}", ident));
        }
    }

    for (use_item, path) in [
        ("ClassHashSerde", "starknet::class_hash::ClassHashSerde"),
        ("ContractAddressSerde", "starknet::contract_address::ContractAddressSerde"),
        ("StorageAddressSerde", "starknet::storage_access::StorageAddressSerde"),
        ("OptionTrait", "option::OptionTrait"),
        ("OptionTraitImpl", "option::OptionTraitImpl"),
    ]
    .into_iter()
    {
        extra_uses.entry(use_item.into()).or_insert_with(|| path.to_string());
    }

    let extra_uses_node = RewriteNode::new_modified(
        extra_uses
            .values()
            .map(|use_path| RewriteNode::Text(format!("\n        use {use_path};")))
            .collect(),
    );

    let mut data = ContractGenerationData::default();

    let mut storage_code = RewriteNode::Text("".to_string());
    for item in body.items(db).elements(db) {
        match &item {
            ast::Item::FreeFunction(item_function) if item_function.has_attr(db, EVENT_ATTR) => {
                let (rewrite_nodes, event_diagnostics) = handle_event(db, item_function.clone());
                if let Some((event_function_rewrite, abi_event_rewrite)) = rewrite_nodes {
                    data.event_functions.push(event_function_rewrite);
                    data.abi_events.push(abi_event_rewrite);
                }
                diagnostics.extend(event_diagnostics);
            }
            ast::Item::FreeFunction(item_function) => {
                let Some(entry_point_kind) = EntryPointKind::try_from_function_with_body(db, item_function) else {
                    continue;
                };
                let function_name = RewriteNode::new_trimmed(
                    item_function.declaration(db).name(db).as_syntax_node(),
                );
                handle_entry_point(
                    entry_point_kind,
                    item_function,
                    function_name,
                    db,
                    &mut diagnostics,
                    &mut data,
                );
            }
            ast::Item::Impl(item_impl) => {
                if !item_impl.has_attr(db, EXTERNAL_ATTR) {
                    continue;
                }
                let ast::MaybeImplBody::Some(body) = item_impl.body(db) else { continue; };
                let impl_name = RewriteNode::new_trimmed(item_impl.name(db).as_syntax_node());
                for item in body.items(db).elements(db) {
                    let ast::ImplItem::Function(item_function) = item else { continue; };
                    let function_name = RewriteNode::new_trimmed(
                        item_function.declaration(db).name(db).as_syntax_node(),
                    );
                    let function_name = RewriteNode::interpolate_patched(
                        "$impl_name$::$func_name$",
                        [
                            ("impl_name".to_string(), impl_name.clone()),
                            ("func_name".to_string(), function_name),
                        ]
                        .into(),
                    );
                    handle_entry_point(
                        EntryPointKind::External,
                        &item_function,
                        function_name,
                        db,
                        &mut diagnostics,
                        &mut data,
                    );
                }
            }
            ast::Item::Struct(item_struct)
                if item_struct.name(db).text(db) == STORAGE_STRUCT_NAME =>
            {
                let (storage_rewrite_node, storage_diagnostics) =
                    handle_storage_struct(db, item_struct.clone(), &extra_uses_node, has_event);
                storage_code = storage_rewrite_node;
                diagnostics.extend(storage_diagnostics);
            }
            _ => {}
        }
    }

    let module_name_ast = module_ast.name(db);
    let test_class_hash = starknet_keccak(
        module_ast.as_syntax_node().get_text_without_trivia(db).as_str().as_bytes(),
    );
    let generated_contract_mod = RewriteNode::interpolate_patched(
        formatdoc!(
            "
            use starknet::SyscallResultTrait;
            use starknet::SyscallResultTraitImpl;

            const TEST_CLASS_HASH: felt252 = {test_class_hash};
            $storage_code$

            $event_functions$

            trait {ABI_TRAIT}<Storage> {{
                $abi_functions$
                $abi_events$
            }}

            mod {EXTERNAL_MODULE} {{$extra_uses$

                $generated_external_functions$
            }}

            mod {L1_HANDLER_MODULE} {{$extra_uses$

                $generated_l1_handler_functions$
            }}

            mod {CONSTRUCTOR_MODULE} {{$extra_uses$

                $generated_constructor_functions$
            }}
        "
        )
        .as_str(),
        [
            (
                "contract_name".to_string(),
                RewriteNode::new_trimmed(module_name_ast.as_syntax_node()),
            ),
            ("original_items".to_string(), RewriteNode::new_modified(kept_original_items)),
            ("storage_code".to_string(), storage_code),
            ("event_functions".to_string(), RewriteNode::new_modified(data.event_functions)),
            ("abi_functions".to_string(), RewriteNode::new_modified(data.abi_functions)),
            ("abi_events".to_string(), RewriteNode::new_modified(data.abi_events)),
            ("extra_uses".to_string(), extra_uses_node),
            (
                "generated_external_functions".to_string(),
                RewriteNode::new_modified(data.generated_external_functions),
            ),
            (
                "generated_l1_handler_functions".to_string(),
                RewriteNode::new_modified(data.generated_l1_handler_functions),
            ),
            (
                "generated_constructor_functions".to_string(),
                RewriteNode::new_modified(data.generated_constructor_functions),
            ),
        ]
        .into(),
    );

    let mut builder = PatchBuilder::new(db);
    builder.add_modified(generated_contract_mod);
    Some(PluginResult {
        code: Some(PluginGeneratedFile {
            name: "contract".into(),
            content: builder.code,
            aux_data: DynGeneratedFileAuxData::new(DynPluginAuxData::new(
                StarkNetContractAuxData {
                    patches: builder.patches,
                    contracts: vec![module_name_ast.text(db)],
                },
            )),
        }),
        diagnostics,
        remove_original_item: true,
    })
}

/// Handles a contract entrypoint function.
fn handle_entry_point(
    entry_point_kind: EntryPointKind,
    item_function: &ast::FunctionWithBody,
    function_name: RewriteNode,
    db: &dyn SyntaxGroup,
    diagnostics: &mut Vec<PluginDiagnostic>,
    data: &mut ContractGenerationData,
) {
    let attr = entry_point_kind.get_attr();

    let declaration = item_function.declaration(db);
    if let OptionWrappedGenericParamList::WrappedGenericParamList(generic_params) =
        declaration.generic_params(db)
    {
        diagnostics.push(PluginDiagnostic {
            message: "Contract entry points cannot have generic arguments".to_string(),
            stable_ptr: generic_params.stable_ptr().untyped(),
        })
    }

    // TODO(ilya): Validate that an account contract has all the required functions.

    let mut declaration_node = RewriteNode::new_trimmed(declaration.as_syntax_node());
    let original_parameters = declaration_node
        .modify_child(db, ast::FunctionDeclaration::INDEX_SIGNATURE)
        .modify_child(db, ast::FunctionSignature::INDEX_PARAMETERS);
    let params = declaration.signature(db).parameters(db);
    for (param_idx, param) in params.elements(db).iter().enumerate() {
        // This assumes `mut` can only appear alone.
        if is_mut_param(db, param) {
            original_parameters
                .modify_child(db, param_idx * 2)
                .modify_child(db, ast::Param::INDEX_MODIFIERS)
                .set_str("".to_string());
        }
    }
    data.abi_functions.push(RewriteNode::new_modified(vec![
        RewriteNode::Text(format!("#[{attr}]\n        ")),
        declaration_node,
        RewriteNode::Text(";\n        ".to_string()),
    ]));

    match generate_entry_point_wrapper(db, item_function, function_name) {
        Ok(generated_function) => {
            let generated = match entry_point_kind {
                EntryPointKind::Constructor => &mut data.generated_constructor_functions,
                EntryPointKind::L1Handler => {
                    validate_l1_handler_first_parameter(db, &params, diagnostics);
                    &mut data.generated_l1_handler_functions
                }
                EntryPointKind::External => &mut data.generated_external_functions,
            };
            generated.push(generated_function);
            generated.push(RewriteNode::Text("\n        ".to_string()));
        }
        Err(entry_point_diagnostics) => {
            diagnostics.extend(entry_point_diagnostics);
        }
    }
}

/// Validates the first parameter of an L1 handler is `from_address: felt252` or `_from_address:
/// felt252`.
fn validate_l1_handler_first_parameter(
    db: &dyn SyntaxGroup,
    params: &ast::ParamList,
    diagnostics: &mut Vec<PluginDiagnostic>,
) {
    if let Some(first_param) = params.elements(db).get(1) {
        // Validate type
        if !is_felt252(db, &first_param.type_clause(db).ty(db)) {
            diagnostics.push(PluginDiagnostic {
                message: "The second parameter of an L1 handler must be of type `felt252`."
                    .to_string(),
                stable_ptr: first_param.stable_ptr().untyped(),
            });
        }

        // Validate name
        if maybe_strip_underscore(first_param.name(db).text(db).as_str())
            != L1_HANDLER_FIRST_PARAM_NAME
        {
            diagnostics.push(PluginDiagnostic {
                message: "The second parameter of an L1 handler must be named 'from_address'."
                    .to_string(),
                stable_ptr: first_param.stable_ptr().untyped(),
            });
        }
    } else {
        diagnostics.push(PluginDiagnostic {
            message: "An L1 handler must have the 'from_address' as its second parameter."
                .to_string(),
            stable_ptr: params.stable_ptr().untyped(),
        });
    };
}
