//! A `Compilation` contains the compiled function bodies for a WebAssembly
//! module

use super::module::Module;
use super::instance::Instance;
use super::{Relocations, Relocation, DataInitializer};
use cretonne::{self, result::CtonError, isa::TargetIsa};
use super::RelocSink;
use super::abi::ABI_MAP;

use memory::{Code, Region, sip};

use nabi::{Result, Error};

use core::slice;
use core::ptr::{write_unaligned, NonNull};
use alloc::{Vec, String};

extern "C" fn test_external_func(arg: u64) {
    println!("Called from wasm! arg = {}", arg);
}

#[derive(Debug)]
enum FunctionType {
    Local {
        offset: usize,
        size: usize,
    },
    External {
        module: String,
        name: String,
    }
}

#[derive(Debug)]
pub struct Compilation {
    /// The module this is instantiated from
    module: Module,

    instance: Instance,

    region: Region,

    /// Compiled machine code for the function bodies
    /// This is mapped onto `self.region`.
    functions: Vec<FunctionType>,

    first_local_function: usize,

    /// The computed relocations
    relocations: Relocations,
}

impl Compilation {
    /// Allocates the runtime data structures with the given flags
    fn new(module: Module, region: Region, functions: Vec<FunctionType>, relocations: Relocations, instance: Instance) -> Self {
        let first_local_function = functions
            .iter()
            .position(|f| match f {
                FunctionType::Local {..} => true,
                _ => false,
            }).unwrap();

        Compilation {
            module,
            region,
            instance,
            functions,
            first_local_function,
            relocations,
        }
    }

    /// Relocate the compliation.
    fn relocate(&mut self) -> Result<()> {
        // The relocations are relative to the relocation's address plus four bytes
        // TODO: Support architectures other than x86_64, and other reloc kinds.
        for (i, function_relocs) in self.relocations.iter().enumerate() {
            for ref r in function_relocs {
                let (target_func_addr, is_local) = self.get_function_addr(r.func_index)?;
                // let target_func_addr: isize = self.get_function_addr(r.func_index) as isize;
                let body_addr = self.get_function_addr(i + self.first_local_function)?.0;

                let (reloc_addr, reloc_delta) = if is_local {
                    let reloc_addr = unsafe { body_addr.offset(r.offset as isize) as isize };
                    let reloc_addend = r.addend as isize - 4;
                    let reloc_delta = (target_func_addr as isize - reloc_addr + reloc_addend) as i32;
                    (reloc_addr, reloc_delta)
                } else {
                    let reloc_addr = unsafe { body_addr.offset(r.offset as isize) as isize };
                    (reloc_addr, target_func_addr as i32)
                };

                unsafe {
                    write_unaligned(reloc_addr as *mut i32, reloc_delta);
                }
            }
        }

        Ok(())
    }

    fn get_function_addr(&self, index: usize) -> Result<(*const u8, bool)> {
        match self.functions[index] {
            FunctionType::Local {
                offset,
                size,
            } => {
                Ok(((self.region.start().as_u64() as usize + offset) as *const u8, true))
            },
            FunctionType::External {
                ref module,
                ref name,
            } => {
                // TODO: Lookup `module` and `name` to find external address
                // For now, hardcode to single module
                // TODO: Design an api surface
                if module != "abi" {
                    return Err(Error::INTERNAL);
                }

                let abi_func = ABI_MAP.get(name.as_str())?;

                let imported_sig = &self.module.signatures[index];

                if !abi_func.same_sig(imported_sig) {
                    println!("Incorrect signature");
                    println!("ABI sig: {:?}", abi_func);
                    println!("Import sig: {:?}", imported_sig);
                    return Err(Error::INTERNAL);
                }

                Ok((abi_func.ptr, false))
            }
        }
    } 

    /// Emit a `Code` instance
    pub fn emit(mut self) -> Result<Code> {
        self.relocate()?;

        let vmctx = self.instance.generate_vmctx();

        let start_index = self.module.start_func?;
        let start_ptr = self.get_function_addr(start_index)?.0;

        Ok(Code::new(self.module, self.region, self.instance, vmctx, start_ptr))
    }
}

/// Define functions, etc and then "compile"
/// it all into a `Compliation`.
pub struct Compiler<'isa> {
    isa: &'isa TargetIsa,

    contexts: Vec<(cretonne::Context, usize)>,

    total_size: usize,
}

impl<'isa> Compiler<'isa> {
    pub fn new(isa: &'isa TargetIsa) -> Self {
        Self::with_capacity(isa, 0)
    }

    pub fn with_capacity(isa: &'isa TargetIsa, capacity: usize) -> Self {
        Compiler {
            isa,
            contexts: Vec::with_capacity(capacity),
            total_size: 0,
        }
    }

    /// Define a function. This also compiles the function.
    pub fn define_function(&mut self, mut ctx: cretonne::Context) -> Result<()> {
        let code_size = ctx.compile(self.isa)
            .map_err(|e| {
                println!("Compile error: {:?}", e);
                Error::INTERNAL
            })? as usize;

        self.contexts.push((ctx, code_size));

        self.total_size += code_size;

        Ok(())
    }
    
    /// This allocates a region from the Sip memory allocator
    /// and emits all the functions into that.
    /// 
    /// This assumes that the functions don't need a specific
    /// alignment, which is true on x86_64, but may not
    /// be true on other architectures.
    pub fn compile(self, module: Module, data_initializers: &[DataInitializer]) -> Result<Compilation> {
        let mut region = sip::allocate_region(self.total_size)
            .ok_or(Error::NO_MEMORY)?;

        let mut functions = Vec::with_capacity(module.functions.len());
        let mut relocs = Vec::with_capacity(self.contexts.len());

        let mut offset = 0;
        let region_start = region.start().as_u64() as usize;

        for (module, name) in module.imported_funcs.iter().cloned() {
            functions.push(FunctionType::External {
                module,
                name,
            });
        }
        
        // emit functions to memory
        for (ref ctx, size) in self.contexts.iter() {
            let mut reloc_sink = RelocSink::new(&ctx.func);
            ctx.emit_to_memory((region_start + offset) as *mut u8, &mut reloc_sink, self.isa);
            functions.push(FunctionType::Local {
                offset,
                size: *size,
            });
            relocs.push(reloc_sink.func_relocs);

            offset += size;
        }

        let instance = Instance::new(&module, data_initializers);

        Ok(Compilation::new(module, region, functions, relocs, instance))
    }
}