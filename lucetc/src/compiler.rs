mod cpu_features;

pub use self::cpu_features::{CpuFeatures, SpecificFeature, TargetCpu};
use crate::decls::ModuleDecls;
use crate::error::Error;
use crate::function::FuncInfo;
use crate::heap::HeapSettings;
use crate::module::ModuleInfo;
use crate::output::{CraneliftFuncs, ObjectFile, FUNCTION_MANIFEST_SYM};
use crate::runtime::Runtime;
use crate::stack_probe;
use crate::table::write_table_data;
use crate::traps::{translate_trapcode, trap_sym_for_func};
use byteorder::{LittleEndian, WriteBytesExt};
use cranelift_codegen::{
    binemit, ir,
    isa::TargetIsa,
    settings::{self, Configurable},
    Context as ClifContext,
};
use cranelift_module::{
    Backend as ClifBackend, DataContext as ClifDataContext, DataId, FuncId, FuncOrDataId,
    Linkage as ClifLinkage, Module as ClifModule,
};
use cranelift_object::{ObjectBackend, ObjectBuilder};
use cranelift_wasm::{translate_module, FuncTranslator, ModuleTranslationState, WasmError};
use lucet_module::bindings::Bindings;
use lucet_module::{
    ModuleData, ModuleFeatures, SerializedModule, VersionInfo, LUCET_MODULE_SYM, MODULE_DATA_SYM,
};
use lucet_validate::Validator;
use std::collections::HashMap;
use std::io::Cursor;
use target_lexicon::Triple;

#[derive(Debug, Clone, Copy)]
pub enum OptLevel {
    None,
    Speed,
    SpeedAndSize,
}

impl Default for OptLevel {
    fn default() -> OptLevel {
        OptLevel::SpeedAndSize
    }
}

impl OptLevel {
    pub fn to_flag(&self) -> &str {
        match self {
            OptLevel::None => "none",
            OptLevel::Speed => "speed",
            OptLevel::SpeedAndSize => "speed_and_size",
        }
    }
}

pub struct CompilerBuilder {
    target: Triple,
    opt_level: OptLevel,
    cpu_features: CpuFeatures,
    heap_settings: HeapSettings,
    count_instructions: bool,
    canonicalize_nans: bool,
    validator: Option<Validator>,
    blade_type: String,
    blade_v1_1: bool,
}

impl CompilerBuilder {
    pub fn new() -> Self {
        Self {
            target: Triple::host(),
            opt_level: OptLevel::default(),
            cpu_features: CpuFeatures::default(),
            heap_settings: HeapSettings::default(),
            count_instructions: false,
            canonicalize_nans: false,
            validator: None,
            blade_type: "none".into(),
            blade_v1_1: false,
        }
    }

    pub(crate) fn target_ref(&self) -> &Triple {
        &self.target
    }

    pub fn target(&mut self, target: Triple) {
        self.target = target;
    }

    pub fn with_target(mut self, target: Triple) -> Self {
        self.target(target);
        self
    }

    pub fn opt_level(&mut self, opt_level: OptLevel) {
        self.opt_level = opt_level;
    }

    pub fn with_opt_level(mut self, opt_level: OptLevel) -> Self {
        self.opt_level(opt_level);
        self
    }

    pub fn cpu_features(&mut self, cpu_features: CpuFeatures) {
        self.cpu_features = cpu_features;
    }

    pub fn with_cpu_features(mut self, cpu_features: CpuFeatures) -> Self {
        self.cpu_features(cpu_features);
        self
    }

    pub fn cpu_features_mut(&mut self) -> &mut CpuFeatures {
        &mut self.cpu_features
    }

    pub fn heap_settings(&mut self, heap_settings: HeapSettings) {
        self.heap_settings = heap_settings;
    }

    pub fn with_heap_settings(mut self, heap_settings: HeapSettings) -> Self {
        self.heap_settings(heap_settings);
        self
    }

    pub fn heap_settings_mut(&mut self) -> &mut HeapSettings {
        &mut self.heap_settings
    }

    pub fn count_instructions(&mut self, count_instructions: bool) {
        self.count_instructions = count_instructions;
    }

    pub fn with_count_instructions(mut self, count_instructions: bool) -> Self {
        self.count_instructions(count_instructions);
        self
    }

    pub fn canonicalize_nans(&mut self, canonicalize_nans: bool) {
        self.canonicalize_nans = canonicalize_nans;
    }

    pub fn with_canonicalize_nans(mut self, canonicalize_nans: bool) -> Self {
        self.canonicalize_nans(canonicalize_nans);
        self
    }

    pub fn validator(&mut self, validator: Option<Validator>) {
        self.validator = validator;
    }

    pub fn with_validator(mut self, validator: Option<Validator>) -> Self {
        self.validator(validator);
        self
    }

    pub fn blade_type(&mut self, blade_type: String) {
        self.blade_type = blade_type;
    }

    pub fn with_blade_type(mut self, blade_type: String) -> Self {
        self.blade_type(blade_type);
        self
    }

    pub fn blade_v1_1(&mut self, blade_v1_1: bool) {
        self.blade_v1_1 = blade_v1_1;
    }

    pub fn with_blade_v1_1(mut self, blade_v1_1: bool) -> Self {
        self.blade_v1_1(blade_v1_1);
        self
    }

    pub fn create<'a>(
        &'a self,
        wasm_binary: &'a [u8],
        bindings: &'a Bindings,
    ) -> Result<Compiler<'a>, Error> {
        Compiler::new(
            wasm_binary,
            self.target.clone(),
            self.opt_level,
            self.cpu_features.clone(),
            bindings,
            self.heap_settings.clone(),
            self.count_instructions,
            &self.validator,
            self.canonicalize_nans,
            self.blade_type.clone(),
            self.blade_v1_1,
        )
    }
}

pub struct Compiler<'a> {
    decls: ModuleDecls<'a>,
    clif_module: ClifModule<ObjectBackend>,
    target: Triple,
    opt_level: OptLevel,
    cpu_features: CpuFeatures,
    count_instructions: bool,
    module_translation_state: ModuleTranslationState,
    canonicalize_nans: bool,
    blade_type: String,
    blade_v1_1: bool,
}

impl<'a> Compiler<'a> {
    pub fn new(
        wasm_binary: &'a [u8],
        target: Triple,
        opt_level: OptLevel,
        cpu_features: CpuFeatures,
        bindings: &'a Bindings,
        heap_settings: HeapSettings,
        count_instructions: bool,
        validator: &Option<Validator>,
        canonicalize_nans: bool,
        blade_type: String,
        blade_v1_1: bool,
    ) -> Result<Self, Error> {
        let isa = Self::target_isa(target.clone(), opt_level, &cpu_features, canonicalize_nans, &blade_type, blade_v1_1)?;

        let frontend_config = isa.frontend_config();
        let mut module_info = ModuleInfo::new(frontend_config.clone());

        if let Some(v) = validator {
            v.validate(wasm_binary).map_err(Error::LucetValidation)?;
        } else {
            // As of cranelift-wasm 0.43 which uses wasmparser 0.39.1, the parser used inside
            // cranelift-wasm does not validate. We need to run the validating parser on the binary
            // first. The InvalidWebAssembly error below will never trigger.
            wasmparser::validate(wasm_binary, None).map_err(Error::WasmValidation)?;
        }

        let module_translation_state =
            translate_module(wasm_binary, &mut module_info).map_err(|e| match e {
                WasmError::User(u) => Error::Input(u),
                WasmError::InvalidWebAssembly { .. } => {
                    // Since wasmparser was already used to validate,
                    // reaching this case means there's a significant
                    // bug in either wasmparser or cranelift-wasm.
                    unreachable!();
                }
                WasmError::Unsupported(s) => Error::Unsupported(s),
                WasmError::ImplLimitExceeded { .. } => Error::ClifWasmError(e),
            })?;

        let libcalls = Box::new(move |libcall| match libcall {
            ir::LibCall::Probestack => stack_probe::STACK_PROBE_SYM.to_owned(),
            _ => (cranelift_module::default_libcall_names())(libcall),
        });

        let mut builder = ObjectBuilder::new(isa, "lucet_guest".to_owned(), libcalls);
        builder.function_alignment(16);
        let mut clif_module: ClifModule<ObjectBackend> = ClifModule::new(builder);

        let runtime = Runtime::lucet(frontend_config);
        let decls = ModuleDecls::new(
            module_info,
            &mut clif_module,
            bindings,
            runtime,
            heap_settings,
        )?;

        Ok(Self {
            decls,
            clif_module,
            opt_level,
            cpu_features,
            count_instructions,
            module_translation_state,
            target,
            canonicalize_nans,
            blade_type,
            blade_v1_1,
        })
    }

    pub fn builder() -> CompilerBuilder {
        CompilerBuilder::new()
    }

    pub fn module_features(&self) -> ModuleFeatures {
        let mut mf: ModuleFeatures = (&self.cpu_features).into();
        mf.instruction_count = self.count_instructions;
        mf
    }

    pub fn module_data(&self) -> Result<ModuleData<'_>, Error> {
        self.decls.get_module_data(self.module_features())
    }

    pub fn object_file(mut self) -> Result<ObjectFile, Error> {
        let mut func_translator = FuncTranslator::new();
        let mut function_manifest_ctx = ClifDataContext::new();
        let mut function_manifest_bytes = Cursor::new(Vec::new());
        let mut function_map: HashMap<FuncId, (u32, DataId, usize)> = HashMap::new();

        for (ref func, (code, code_offset)) in self.decls.function_bodies() {
            let mut func_info = FuncInfo::new(&self.decls, self.count_instructions);
            let mut clif_context = ClifContext::new();
            clif_context.func.name = func.name.as_externalname();
            clif_context.func.signature = func.signature.clone();

            func_translator
                .translate(
                    &self.module_translation_state,
                    code,
                    *code_offset,
                    &mut clif_context.func,
                    &mut func_info,
                )
                .map_err(|source| Error::FunctionTranslation {
                    symbol: func.name.symbol().to_string(),
                    source,
                })?;
            let func_id = func.name.as_funcid().unwrap();
            let mut traps = TrapSites::new();
            let compiled = self
                .clif_module
                .define_function(func_id, &mut clif_context, &mut traps)
                .map_err(|source| Error::FunctionDefinition {
                    symbol: func.name.symbol().to_string(),
                    source,
                })?;

            let size = compiled.size;

            let trap_data_id = traps.write(&mut self.clif_module, func.name.symbol())?;

            function_map.insert(func_id, (size, trap_data_id, traps.len()));
        }

        // Write out the stack probe and associated data.
        let probe_id = stack_probe::declare(&mut self.decls, &mut self.clif_module)?;
        let probe_func = self.decls.get_func(probe_id).unwrap();
        let probe_func_id = probe_func.name.as_funcid().unwrap();
        let compiled = self
            .clif_module
            .define_function_bytes(probe_func_id, stack_probe::STACK_PROBE_BINARY)?;

        let size = compiled.size;
        let stack_probe_traps: TrapSites = stack_probe::trap_sites().into();

        let trap_data_id =
            stack_probe_traps.write(&mut self.clif_module, probe_func.name.symbol())?;

        function_map.insert(probe_func_id, (size, trap_data_id, stack_probe_traps.len()));

        let module_data_bytes = self.module_data()?.serialize()?;

        let module_data_len = module_data_bytes.len();

        let module_data_id = write_module_data(&mut self.clif_module, module_data_bytes)?;
        write_startfunc_data(&mut self.clif_module, &self.decls)?;
        let (table_id, table_len) = write_table_data(&mut self.clif_module, &self.decls)?;

        // The function manifest must be written out in the order that
        // cranelift-module is going to lay out the functions.  We also
        // have to be careful to write function manifest entries for VM
        // functions, which will not be represented in function_map.

        let ids: Vec<FuncId> = self
            .clif_module
            .declared_functions()
            .map(|f| {
                let func_id = match self.clif_module.get_name(&f.decl.name).unwrap() {
                    FuncOrDataId::Func(id) => id,
                    _ => panic!(),
                };
                func_id
            })
            .collect();
        let function_manifest_len = ids.len();

        for func_id in ids {
            let (size, trap_data_id, traps_len) = match function_map.get(&func_id) {
                Some((ref size, ref trap_data_id, ref traps_len)) => {
                    (*size, Some(*trap_data_id), *traps_len)
                }
                None => (0 as u32, None, 0 as usize),
            };

            write_function_spec(
                &mut self.clif_module,
                &mut function_manifest_ctx,
                &mut function_manifest_bytes,
                func_id,
                size,
                trap_data_id,
                traps_len,
            )?;
        }

        function_manifest_ctx.define(function_manifest_bytes.into_inner().into());
        let manifest_data_id = self.clif_module.declare_data(
            FUNCTION_MANIFEST_SYM,
            ClifLinkage::Local,
            false,
            false,
            None,
        )?;
        self.clif_module
            .define_data(manifest_data_id, &function_manifest_ctx)?;

        // Write out the structure tying everything together.
        let mut native_data =
            Cursor::new(Vec::with_capacity(std::mem::size_of::<SerializedModule>()));
        let mut native_data_ctx = ClifDataContext::new();
        let native_data_id = self.clif_module.declare_data(
            LUCET_MODULE_SYM,
            ClifLinkage::Export,
            false,
            false,
            None,
        )?;

        let version =
            VersionInfo::current(include_str!(concat!(env!("OUT_DIR"), "/commit_hash")).as_bytes());

        version.write_to(&mut native_data)?;

        fn write_slice(
            module: &mut ClifModule<ObjectBackend>,
            mut ctx: &mut ClifDataContext,
            bytes: &mut Cursor<Vec<u8>>,
            id: DataId,
            len: usize,
        ) -> Result<(), Error> {
            let data_ref = module.declare_data_in_data(id, &mut ctx);
            let offset = bytes.position() as u32;
            ctx.write_data_addr(offset, data_ref, 0);
            bytes.write_u64::<LittleEndian>(0 as u64)?;
            bytes.write_u64::<LittleEndian>(len as u64)?;
            Ok(())
        }

        write_slice(
            &mut self.clif_module,
            &mut native_data_ctx,
            &mut native_data,
            module_data_id,
            module_data_len,
        )?;
        write_slice(
            &mut self.clif_module,
            &mut native_data_ctx,
            &mut native_data,
            table_id,
            table_len,
        )?;
        write_slice(
            &mut self.clif_module,
            &mut native_data_ctx,
            &mut native_data,
            manifest_data_id,
            function_manifest_len,
        )?;

        native_data_ctx.define(native_data.into_inner().into());
        self.clif_module
            .define_data(native_data_id, &native_data_ctx)?;

        let obj = ObjectFile::new(self.clif_module.finish())?;

        Ok(obj)
    }

    pub fn cranelift_funcs(self) -> Result<CraneliftFuncs, Error> {
        let mut funcs = HashMap::new();
        let mut func_translator = FuncTranslator::new();

        for (ref func, (code, code_offset)) in self.decls.function_bodies() {
            let mut func_info = FuncInfo::new(&self.decls, self.count_instructions);
            let mut clif_context = ClifContext::new();
            clif_context.func.name = func.name.as_externalname();
            clif_context.func.signature = func.signature.clone();

            func_translator
                .translate(
                    &self.module_translation_state,
                    code,
                    *code_offset,
                    &mut clif_context.func,
                    &mut func_info,
                )
                .map_err(|source| Error::FunctionTranslation {
                    symbol: func.name.symbol().to_string(),
                    source,
                })?;

            funcs.insert(func.name.clone(), clif_context.func);
        }
        Ok(CraneliftFuncs::new(
            funcs,
            Self::target_isa(
                self.target,
                self.opt_level,
                &self.cpu_features,
                self.canonicalize_nans,
                self.blade_type,
                self.blade_v1_1,
            )?,
        ))
    }

    fn target_isa(
        target: Triple,
        opt_level: OptLevel,
        cpu_features: &CpuFeatures,
        canonicalize_nans: bool,
        blade_type: impl AsRef<str>,
        blade_v1_1: bool,
    ) -> Result<Box<dyn TargetIsa>, Error> {
        let mut flags_builder = settings::builder();
        let isa_builder = cpu_features.isa_builder(target)?;
        flags_builder.enable("enable_verifier").unwrap();
        flags_builder.enable("is_pic").unwrap();
        flags_builder.set("opt_level", opt_level.to_flag()).unwrap();
        flags_builder.set("blade_type", blade_type.as_ref()).unwrap();
        if canonicalize_nans {
            flags_builder.enable("enable_nan_canonicalization").unwrap();
        }
        if blade_v1_1 {
            flags_builder.enable("blade_v1_1").unwrap();
        }
        Ok(isa_builder.finish(settings::Flags::new(flags_builder)))
    }
}

fn write_module_data<B: ClifBackend>(
    clif_module: &mut ClifModule<B>,
    module_data_bytes: Vec<u8>,
) -> Result<DataId, Error> {
    use cranelift_module::{DataContext, Linkage};

    let mut module_data_ctx = DataContext::new();
    module_data_ctx.define(module_data_bytes.into_boxed_slice());

    let module_data_decl = clif_module
        .declare_data(MODULE_DATA_SYM, Linkage::Local, true, false, None)
        .map_err(Error::ClifModuleError)?;
    clif_module
        .define_data(module_data_decl, &module_data_ctx)
        .map_err(Error::ClifModuleError)?;

    Ok(module_data_decl)
}

fn write_startfunc_data<B: ClifBackend>(
    clif_module: &mut ClifModule<B>,
    decls: &ModuleDecls<'_>,
) -> Result<(), Error> {
    use cranelift_module::{DataContext, Linkage};

    if let Some(func_ix) = decls.get_start_func() {
        let name = clif_module
            .declare_data("guest_start", Linkage::Export, false, false, None)
            .map_err(Error::MetadataSerializer)?;
        let mut ctx = DataContext::new();
        ctx.define(vec![0u8; 8].into_boxed_slice());

        let start_func = decls
            .get_func(func_ix)
            .expect("start func is valid func id");
        let fid = start_func.name.as_funcid().expect("start func is a func");
        let fref = clif_module.declare_func_in_data(fid, &mut ctx);
        ctx.write_function_addr(0, fref);
        clif_module
            .define_data(name, &ctx)
            .map_err(Error::MetadataSerializer)?;
    }
    Ok(())
}

/// Collect traps from cranelift_module codegen:
struct TrapSites {
    traps: Vec<cranelift_module::TrapSite>,
}

/// Convert from representation in stack_probe:
impl From<Vec<cranelift_module::TrapSite>> for TrapSites {
    fn from(traps: Vec<cranelift_module::TrapSite>) -> TrapSites {
        TrapSites { traps }
    }
}

impl TrapSites {
    /// Empty
    fn new() -> Self {
        Self { traps: Vec::new() }
    }
    /// Serialize for lucet_module:
    fn serialize(&self) -> Box<[u8]> {
        let traps: Vec<lucet_module::TrapSite> = self
            .traps
            .iter()
            .map(|site| lucet_module::TrapSite {
                offset: site.offset,
                code: translate_trapcode(site.code),
            })
            .collect();

        let trap_site_bytes = unsafe {
            std::slice::from_raw_parts(
                traps.as_ptr() as *const u8,
                traps.len() * std::mem::size_of::<lucet_module::TrapSite>(),
            )
        };

        trap_site_bytes.to_vec().into()
    }
    /// Write traps for a given function into the cranelift module:
    pub fn write(
        &self,
        module: &mut ClifModule<ObjectBackend>,
        func_name: &str,
    ) -> Result<DataId, Error> {
        let trap_sym = trap_sym_for_func(func_name);
        let mut trap_sym_ctx = ClifDataContext::new();
        trap_sym_ctx.define(self.serialize());

        let trap_data_id =
            module.declare_data(&trap_sym, ClifLinkage::Local, false, false, None)?;

        module.define_data(trap_data_id, &trap_sym_ctx)?;

        Ok(trap_data_id)
    }
    pub fn len(&self) -> usize {
        self.traps.len()
    }
}

impl cranelift_codegen::binemit::TrapSink for TrapSites {
    fn trap(
        &mut self,
        offset: cranelift_codegen::binemit::CodeOffset,
        srcloc: cranelift_codegen::ir::SourceLoc,
        code: cranelift_codegen::ir::TrapCode,
    ) {
        self.traps.push(cranelift_module::TrapSite {
            offset,
            srcloc,
            code,
        })
    }
}

fn write_function_spec(
    module: &mut ClifModule<ObjectBackend>,
    mut manifest_ctx: &mut ClifDataContext,
    manifest_bytes: &mut Cursor<Vec<u8>>,
    func_id: FuncId,
    size: binemit::CodeOffset,
    trap_data_id: Option<DataId>,
    n_traps: usize,
) -> Result<(), Error> {
    // This code has implicit knowledge of the layout of `FunctionSpec`!
    //
    // Write a (ptr, len) pair with relocation for the code.
    let func_ref = module.declare_func_in_data(func_id, &mut manifest_ctx);
    let offset = manifest_bytes.position() as u32;
    manifest_ctx.write_function_addr(offset, func_ref);
    manifest_bytes.write_u64::<LittleEndian>(0 as u64)?;
    manifest_bytes.write_u64::<LittleEndian>(size as u64)?;
    // Write a (ptr, len) pair with relocation for the trap table.
    if let Some(trap_data_id) = trap_data_id {
        if n_traps > 0 {
            let data_ref = module.declare_data_in_data(trap_data_id, &mut manifest_ctx);
            let offset = manifest_bytes.position() as u32;
            manifest_ctx.write_data_addr(offset, data_ref, 0);
        }
    }
    manifest_bytes.write_u64::<LittleEndian>(0 as u64)?;
    manifest_bytes.write_u64::<LittleEndian>(n_traps as u64)?;

    Ok(())
}
