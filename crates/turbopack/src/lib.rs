#![feature(box_patterns)]
#![feature(trivial_bounds)]
#![feature(min_specialization)]
#![feature(map_try_insert)]
#![feature(option_get_or_insert_default)]
#![feature(hash_set_entry)]
#![recursion_limit = "256"]
#![feature(arbitrary_self_types)]
#![feature(async_fn_in_trait)]

pub mod condition;
pub mod evaluate_context;
mod graph;
pub mod module_options;
pub mod rebase;
pub mod resolve;
pub mod resolve_options_context;
pub mod transition;
pub(crate) mod unsupported_sass;

use std::{
    collections::{HashMap, HashSet},
    mem::swap,
};

use anyhow::Result;
use css::{CssModuleAsset, GlobalCssAsset, ModuleCssAsset};
use ecmascript::{
    typescript::resolve::TypescriptTypesAssetReference, EcmascriptModuleAsset,
    EcmascriptModuleAssetType,
};
use graph::{aggregate, AggregatedGraph, AggregatedGraphNodeContent};
use module_options::{ModuleOptions, ModuleOptionsContext, ModuleRuleEffect, ModuleType};
pub use resolve::resolve_options;
use turbo_tasks::{Completion, Value, Vc};
use turbo_tasks_fs::FileSystemPath;
use turbopack_core::{
    asset::Asset,
    compile_time_info::CompileTimeInfo,
    context::AssetContext,
    ident::AssetIdent,
    issue::{Issue, IssueExt, StyledString},
    module::Module,
    output::OutputAsset,
    raw_module::RawModule,
    reference_type::{EcmaScriptModulesReferenceSubType, InnerAssets, ReferenceType},
    resolve::{
        options::ResolveOptions, origin::PlainResolveOrigin, parse::Request, resolve,
        AffectingResolvingAssetReference, ModulePart, ModuleResolveResult, ResolveResult,
    },
    source::Source,
};
pub use turbopack_css as css;
pub use turbopack_ecmascript as ecmascript;
use turbopack_json::JsonModuleAsset;
use turbopack_mdx::MdxModuleAsset;
use turbopack_static::StaticModuleAsset;
use turbopack_wasm::{module_asset::WebAssemblyModuleAsset, source::WebAssemblySource};

use self::{
    module_options::CustomModuleType,
    resolve_options_context::ResolveOptionsContext,
    transition::{Transition, TransitionsByName},
};

#[turbo_tasks::value]
struct ModuleIssue {
    ident: Vc<AssetIdent>,
    title: Vc<String>,
    description: Vc<StyledString>,
}

#[turbo_tasks::value_impl]
impl Issue for ModuleIssue {
    #[turbo_tasks::function]
    fn category(&self) -> Vc<String> {
        Vc::cell("other".to_string())
    }

    #[turbo_tasks::function]
    fn file_path(&self) -> Vc<FileSystemPath> {
        self.ident.path()
    }

    #[turbo_tasks::function]
    fn title(&self) -> Vc<String> {
        self.title
    }

    #[turbo_tasks::function]
    fn description(&self) -> Vc<StyledString> {
        self.description
    }
}

#[turbo_tasks::function]
async fn apply_module_type(
    source: Vc<Box<dyn Source>>,
    module_asset_context: Vc<ModuleAssetContext>,
    module_type: Vc<ModuleType>,
    part: Option<Vc<ModulePart>>,
    inner_assets: Option<Vc<InnerAssets>>,
) -> Result<Vc<Box<dyn Module>>> {
    let module_type = &*module_type.await?;
    Ok(match module_type {
        ModuleType::Ecmascript {
            transforms,
            options,
        }
        | ModuleType::Typescript {
            transforms,
            options,
        }
        | ModuleType::TypescriptWithTypes {
            transforms,
            options,
        }
        | ModuleType::TypescriptDeclaration {
            transforms,
            options,
        } => {
            let context_for_module = match module_type {
                ModuleType::TypescriptWithTypes { .. }
                | ModuleType::TypescriptDeclaration { .. } => {
                    module_asset_context.with_types_resolving_enabled()
                }
                _ => module_asset_context,
            };
            let mut builder = EcmascriptModuleAsset::builder(
                source,
                Vc::upcast(context_for_module),
                *transforms,
                *options,
                module_asset_context.compile_time_info(),
            );
            match module_type {
                ModuleType::Ecmascript { .. } => {
                    builder = builder.with_type(EcmascriptModuleAssetType::Ecmascript)
                }
                ModuleType::Typescript { .. } => {
                    builder = builder.with_type(EcmascriptModuleAssetType::Typescript)
                }
                ModuleType::TypescriptWithTypes { .. } => {
                    builder = builder.with_type(EcmascriptModuleAssetType::TypescriptWithTypes)
                }
                ModuleType::TypescriptDeclaration { .. } => {
                    builder = builder.with_type(EcmascriptModuleAssetType::TypescriptDeclaration)
                }
                _ => unreachable!(),
            }

            if let Some(inner_assets) = inner_assets {
                builder = builder.with_inner_assets(inner_assets);
            }

            if options.split_into_parts {
                if let Some(part) = part {
                    builder = builder.with_part(part);
                }
            }

            builder.build()
        }
        ModuleType::Json => Vc::upcast(JsonModuleAsset::new(source)),
        ModuleType::Raw => Vc::upcast(RawModule::new(source)),
        ModuleType::CssGlobal => Vc::upcast(GlobalCssAsset::new(
            source,
            Vc::upcast(module_asset_context),
        )),
        ModuleType::CssModule => Vc::upcast(ModuleCssAsset::new(
            source,
            Vc::upcast(module_asset_context),
        )),
        ModuleType::Css { ty, transforms } => Vc::upcast(CssModuleAsset::new(
            source,
            Vc::upcast(module_asset_context),
            *transforms,
            *ty,
        )),
        ModuleType::Static => Vc::upcast(StaticModuleAsset::new(
            source,
            Vc::upcast(module_asset_context),
        )),
        ModuleType::Mdx {
            transforms,
            options,
        } => Vc::upcast(MdxModuleAsset::new(
            source,
            Vc::upcast(module_asset_context),
            *transforms,
            *options,
        )),
        ModuleType::WebAssembly { source_ty } => Vc::upcast(WebAssemblyModuleAsset::new(
            WebAssemblySource::new(source, *source_ty),
            Vc::upcast(module_asset_context),
        )),
        ModuleType::Custom(custom) => custom.create_module(source, module_asset_context, part),
    })
}

#[turbo_tasks::value]
#[derive(Debug)]
pub struct ModuleAssetContext {
    pub transitions: Vc<TransitionsByName>,
    pub compile_time_info: Vc<CompileTimeInfo>,
    pub module_options_context: Vc<ModuleOptionsContext>,
    pub resolve_options_context: Vc<ResolveOptionsContext>,
    pub layer: Vc<String>,
    transition: Option<Vc<Box<dyn Transition>>>,
}

#[turbo_tasks::value_impl]
impl ModuleAssetContext {
    #[turbo_tasks::function]
    pub fn new(
        transitions: Vc<TransitionsByName>,
        compile_time_info: Vc<CompileTimeInfo>,
        module_options_context: Vc<ModuleOptionsContext>,
        resolve_options_context: Vc<ResolveOptionsContext>,
        layer: Vc<String>,
    ) -> Vc<Self> {
        Self::cell(ModuleAssetContext {
            transitions,
            compile_time_info,
            module_options_context,
            resolve_options_context,
            transition: None,
            layer,
        })
    }

    #[turbo_tasks::function]
    pub fn new_transition(
        transitions: Vc<TransitionsByName>,
        compile_time_info: Vc<CompileTimeInfo>,
        module_options_context: Vc<ModuleOptionsContext>,
        resolve_options_context: Vc<ResolveOptionsContext>,
        layer: Vc<String>,
        transition: Vc<Box<dyn Transition>>,
    ) -> Vc<Self> {
        Self::cell(ModuleAssetContext {
            transitions,
            compile_time_info,
            module_options_context,
            resolve_options_context,
            layer,
            transition: Some(transition),
        })
    }

    #[turbo_tasks::function]
    pub async fn module_options_context(self: Vc<Self>) -> Result<Vc<ModuleOptionsContext>> {
        Ok(self.await?.module_options_context)
    }

    #[turbo_tasks::function]
    pub async fn is_types_resolving_enabled(self: Vc<Self>) -> Result<Vc<bool>> {
        let resolve_options_context = self.await?.resolve_options_context.await?;
        Ok(Vc::cell(
            resolve_options_context.enable_types && resolve_options_context.enable_typescript,
        ))
    }

    #[turbo_tasks::function]
    pub async fn with_types_resolving_enabled(self: Vc<Self>) -> Result<Vc<ModuleAssetContext>> {
        if *self.is_types_resolving_enabled().await? {
            return Ok(self);
        }
        let this = self.await?;
        let resolve_options_context = this
            .resolve_options_context
            .with_types_enabled()
            .resolve()
            .await?;

        Ok(ModuleAssetContext::new(
            this.transitions,
            this.compile_time_info,
            this.module_options_context,
            resolve_options_context,
            this.layer,
        ))
    }

    #[turbo_tasks::function]
    fn process_default(
        self: Vc<Self>,
        source: Vc<Box<dyn Source>>,
        reference_type: Value<ReferenceType>,
    ) -> Vc<Box<dyn Module>> {
        process_default(self, source, reference_type, Vec::new())
    }
}

#[turbo_tasks::function]
async fn process_default(
    module_asset_context: Vc<ModuleAssetContext>,
    source: Vc<Box<dyn Source>>,
    reference_type: Value<ReferenceType>,
    processed_rules: Vec<usize>,
) -> Result<Vc<Box<dyn Module>>> {
    let ident = source.ident().resolve().await?;
    let options = ModuleOptions::new(
        ident.path().parent(),
        module_asset_context.module_options_context(),
    );

    let reference_type = reference_type.into_value();
    let part: Option<Vc<ModulePart>> = match &reference_type {
        ReferenceType::EcmaScriptModules(EcmaScriptModulesReferenceSubType::ImportPart(part)) => {
            Some(*part)
        }
        _ => None,
    };
    let inner_assets = match &reference_type {
        ReferenceType::Internal(inner_assets) => Some(*inner_assets),
        _ => None,
    };
    let mut current_source = source;
    let mut current_module_type = None;
    for (i, rule) in options.await?.rules.iter().enumerate() {
        if processed_rules.contains(&i) {
            continue;
        }
        if rule
            .matches(source, &*ident.path().await?, &reference_type)
            .await?
        {
            for effect in rule.effects() {
                match effect {
                    ModuleRuleEffect::SourceTransforms(transforms) => {
                        current_source = transforms.transform(current_source);
                        if current_source.ident().resolve().await? != ident {
                            // The ident has been changed, so we need to apply new rules.
                            let mut processed_rules = processed_rules.clone();
                            processed_rules.push(i);
                            return Ok(process_default(
                                module_asset_context,
                                current_source,
                                Value::new(reference_type),
                                processed_rules,
                            ));
                        }
                    }
                    ModuleRuleEffect::ModuleType(module) => {
                        current_module_type = Some(*module);
                    }
                    ModuleRuleEffect::AddEcmascriptTransforms(additional_transforms) => {
                        current_module_type = match current_module_type {
                            Some(ModuleType::Ecmascript {
                                transforms,
                                options,
                            }) => Some(ModuleType::Ecmascript {
                                transforms: transforms.extend(*additional_transforms),
                                options,
                            }),
                            Some(ModuleType::Typescript {
                                transforms,
                                options,
                            }) => Some(ModuleType::Typescript {
                                transforms: transforms.extend(*additional_transforms),
                                options,
                            }),
                            Some(ModuleType::TypescriptWithTypes {
                                transforms,
                                options,
                            }) => Some(ModuleType::TypescriptWithTypes {
                                transforms: transforms.extend(*additional_transforms),
                                options,
                            }),
                            Some(module_type) => {
                                ModuleIssue {
                                    ident,
                                    title: Vc::cell("Invalid module type".to_string()),
                                    description: StyledString::String(
                                        "The module type must be Ecmascript or Typescript to add \
                                         Ecmascript transforms"
                                            .to_string(),
                                    )
                                    .cell(),
                                }
                                .cell()
                                .emit();
                                Some(module_type)
                            }
                            None => {
                                ModuleIssue {
                                    ident,
                                    title: Vc::cell("Missing module type".to_string()),
                                    description: StyledString::String(
                                        "The module type effect must be applied before adding \
                                         Ecmascript transforms"
                                            .to_string(),
                                    )
                                    .cell(),
                                }
                                .cell()
                                .emit();
                                None
                            }
                        };
                    }
                }
            }
        }
    }

    let module_type = current_module_type.unwrap_or(ModuleType::Raw).cell();

    Ok(apply_module_type(
        current_source,
        module_asset_context,
        module_type,
        part,
        inner_assets,
    ))
}

#[turbo_tasks::value_impl]
impl AssetContext for ModuleAssetContext {
    #[turbo_tasks::function]
    fn compile_time_info(&self) -> Vc<CompileTimeInfo> {
        self.compile_time_info
    }

    #[turbo_tasks::function]
    fn layer(&self) -> Vc<String> {
        self.layer
    }

    #[turbo_tasks::function]
    async fn resolve_options(
        self: Vc<Self>,
        origin_path: Vc<FileSystemPath>,
        _reference_type: Value<ReferenceType>,
    ) -> Result<Vc<ResolveOptions>> {
        let this = self.await?;
        let module_asset_context = if let Some(transition) = this.transition {
            transition.process_context(self)
        } else {
            self
        };
        // TODO move `apply_commonjs/esm_resolve_options` etc. to here
        Ok(resolve_options(
            origin_path.parent().resolve().await?,
            module_asset_context.await?.resolve_options_context,
        ))
    }

    #[turbo_tasks::function]
    async fn resolve_asset(
        self: Vc<Self>,
        origin_path: Vc<FileSystemPath>,
        request: Vc<Request>,
        resolve_options: Vc<ResolveOptions>,
        reference_type: Value<ReferenceType>,
    ) -> Result<Vc<ModuleResolveResult>> {
        let context_path = origin_path.parent().resolve().await?;

        let result = resolve(context_path, request, resolve_options);
        let mut result = self.process_resolve_result(result.resolve().await?, reference_type);

        if *self.is_types_resolving_enabled().await? {
            let types_reference = TypescriptTypesAssetReference::new(
                Vc::upcast(PlainResolveOrigin::new(Vc::upcast(self), origin_path)),
                request,
            );

            result = result.with_reference(Vc::upcast(types_reference));
        }

        Ok(result)
    }

    #[turbo_tasks::function]
    async fn process_resolve_result(
        self: Vc<Self>,
        result: Vc<ResolveResult>,
        reference_type: Value<ReferenceType>,
    ) -> Result<Vc<ModuleResolveResult>> {
        let this = self.await?;
        let transition = this.transition;
        Ok(result
            .await?
            .map_module(
                |source| {
                    let reference_type = reference_type.clone();
                    async move {
                        if let Some(transition) = transition {
                            Ok(Vc::upcast(
                                transition
                                    .process(source, self, reference_type)
                                    .resolve()
                                    .await?,
                            ))
                        } else {
                            Ok(Vc::upcast(
                                self.process_default(source, reference_type)
                                    .resolve()
                                    .await?,
                            ))
                        }
                    }
                },
                |i| async move { Ok(Vc::upcast(AffectingResolvingAssetReference::new(i))) },
            )
            .await?
            .into())
    }

    #[turbo_tasks::function]
    async fn process(
        self: Vc<Self>,
        asset: Vc<Box<dyn Source>>,
        reference_type: Value<ReferenceType>,
    ) -> Result<Vc<Box<dyn Module>>> {
        let this = self.await?;
        if let Some(transition) = this.transition {
            Ok(transition.process(asset, self, reference_type))
        } else {
            Ok(self.process_default(asset, reference_type))
        }
    }

    #[turbo_tasks::function]
    async fn with_transition(&self, transition: String) -> Result<Vc<Box<dyn AssetContext>>> {
        Ok(
            if let Some(transition) = self.transitions.await?.get(&transition) {
                Vc::upcast(ModuleAssetContext::new_transition(
                    self.transitions,
                    self.compile_time_info,
                    self.module_options_context,
                    self.resolve_options_context,
                    self.layer,
                    *transition,
                ))
            } else {
                // TODO report issue
                Vc::upcast(ModuleAssetContext::new(
                    self.transitions,
                    self.compile_time_info,
                    self.module_options_context,
                    self.resolve_options_context,
                    self.layer,
                ))
            },
        )
    }
}

#[turbo_tasks::function]
pub async fn emit_with_completion(
    asset: Vc<Box<dyn OutputAsset>>,
    output_dir: Vc<FileSystemPath>,
) -> Vc<Completion> {
    emit_assets_aggregated(asset, output_dir)
}

#[turbo_tasks::function]
async fn emit_assets_aggregated(
    asset: Vc<Box<dyn OutputAsset>>,
    output_dir: Vc<FileSystemPath>,
) -> Vc<Completion> {
    let aggregated = aggregate(asset);
    emit_aggregated_assets(aggregated, output_dir)
}

#[turbo_tasks::function]
async fn emit_aggregated_assets(
    aggregated: Vc<AggregatedGraph>,
    output_dir: Vc<FileSystemPath>,
) -> Result<Vc<Completion>> {
    Ok(match &*aggregated.content().await? {
        AggregatedGraphNodeContent::Asset(asset) => emit_asset_into_dir(*asset, output_dir),
        AggregatedGraphNodeContent::Children(children) => {
            for aggregated in children {
                emit_aggregated_assets(*aggregated, output_dir).await?;
            }
            Completion::new()
        }
    })
}

#[turbo_tasks::function]
pub async fn emit_asset(asset: Vc<Box<dyn OutputAsset>>) -> Vc<Completion> {
    asset.content().write(asset.ident().path())
}

#[turbo_tasks::function]
pub async fn emit_asset_into_dir(
    asset: Vc<Box<dyn OutputAsset>>,
    output_dir: Vc<FileSystemPath>,
) -> Result<Vc<Completion>> {
    let dir = &*output_dir.await?;
    Ok(if asset.ident().path().await?.is_inside_ref(dir) {
        emit_asset(asset)
    } else {
        Completion::new()
    })
}

type OutputAssetSet = HashSet<Vc<Box<dyn OutputAsset>>>;

#[turbo_tasks::value(shared)]
struct ReferencesList {
    referenced_by: HashMap<Vc<Box<dyn OutputAsset>>, OutputAssetSet>,
}

#[turbo_tasks::function]
async fn compute_back_references(aggregated: Vc<AggregatedGraph>) -> Result<Vc<ReferencesList>> {
    Ok(match &*aggregated.content().await? {
        &AggregatedGraphNodeContent::Asset(asset) => {
            let mut referenced_by = HashMap::new();
            for reference in asset.references().await?.iter() {
                referenced_by.insert(*reference, [asset].into_iter().collect());
            }
            ReferencesList { referenced_by }.into()
        }
        AggregatedGraphNodeContent::Children(children) => {
            let mut referenced_by = HashMap::<Vc<Box<dyn OutputAsset>>, OutputAssetSet>::new();
            let lists = children
                .iter()
                .map(|child| compute_back_references(*child))
                .collect::<Vec<_>>();
            for list in lists {
                for (key, values) in list.await?.referenced_by.iter() {
                    if let Some(set) = referenced_by.get_mut(key) {
                        for value in values {
                            set.insert(*value);
                        }
                    } else {
                        referenced_by.insert(*key, values.clone());
                    }
                }
            }
            ReferencesList { referenced_by }.into()
        }
    })
}

#[turbo_tasks::function]
async fn top_references(list: Vc<ReferencesList>) -> Result<Vc<ReferencesList>> {
    let list = list.await?;
    const N: usize = 5;
    let mut top = Vec::<(
        &Vc<Box<dyn OutputAsset>>,
        &HashSet<Vc<Box<dyn OutputAsset>>>,
    )>::new();
    for tuple in list.referenced_by.iter() {
        let mut current = tuple;
        for item in &mut top {
            if item.1.len() < tuple.1.len() {
                swap(item, &mut current);
            }
        }
        if top.len() < N {
            top.push(current);
        }
    }
    Ok(ReferencesList {
        referenced_by: top
            .into_iter()
            .map(|(asset, set)| (*asset, set.clone()))
            .collect(),
    }
    .into())
}

pub fn register() {
    turbo_tasks::register();
    turbo_tasks_fs::register();
    turbopack_core::register();
    turbopack_css::register();
    turbopack_ecmascript::register();
    turbopack_node::register();
    turbopack_env::register();
    turbopack_mdx::register();
    turbopack_json::register();
    turbopack_static::register();
    turbopack_wasm::register();
    include!(concat!(env!("OUT_DIR"), "/register.rs"));
}
