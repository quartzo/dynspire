use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Arc;

use crate::kvmap::serialize_kvmap;
use crate::slots::MAX_OUT_SLOTS;

type FnIdlHash = unsafe extern "C" fn() -> u64;
type FnSpierName = unsafe extern "C" fn() -> *const u8;
type FnCreate = unsafe extern "C" fn(*const u8, usize) -> *mut c_void;
type FnDestroy = unsafe extern "C" fn(*mut c_void);
type FnDispatch = unsafe extern "C" fn(
    *mut c_void,
    *const u64,
    usize,
    *mut u64,
    usize,
) -> u8;

pub struct MethodConfig {
    pub name: &'static str,
}

pub struct IdlDescriptor {
    pub hash: u64,
    pub methods: &'static [&'static str],
}

pub trait SpierOp {
    fn as_index(self) -> usize;
}

struct Handle(*mut c_void);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

fn load_sym<T: Copy>(lib: &libloading::Library, name: &[u8]) -> Result<T, String> {
    unsafe {
        let sym: libloading::Symbol<T> = lib
            .get(name)
            .map_err(|e| format!("symbol {}: {e}", String::from_utf8_lossy(name)))?;
        Ok(*sym)
    }
}

pub struct DynSpireLib {
    _lib: libloading::Library,
    dispatch: Arc<Vec<FnDispatch>>,
    create: FnCreate,
    destroy: FnDestroy,
}

unsafe impl Send for DynSpireLib {}
unsafe impl Sync for DynSpireLib {}

impl DynSpireLib {
    pub fn load(
        so_path: &str,
        idl_hash: u64,
        methods: &[MethodConfig],
    ) -> Result<Self, String> {
        let lib = unsafe {
            libloading::Library::new(so_path)
                .map_err(|e| format!("dlopen {}: {e}", so_path))?
        };

        let fn_hash: FnIdlHash = load_sym(&lib, b"dynspire_idl_hash\0")?;
        let hash = unsafe { fn_hash() };
        if hash != idl_hash {
            return Err(format!(
                "IDL hash mismatch: spier=0x{:016x}, expected=0x{:016x}",
                hash, idl_hash
            ));
        }

        let _fn_name: FnSpierName = load_sym(&lib, b"dynspire_spier_name\0")?;
        let fn_create: FnCreate = load_sym(&lib, b"dynspire_create\0")?;
        let fn_destroy: FnDestroy = load_sym(&lib, b"dynspire_destroy\0")?;

        let mut dispatch = Vec::with_capacity(methods.len());
        for config in methods {
            let sym_name = format!("dynspire_dispatch_{}\0", config.name);
            let fn_ptr: FnDispatch = load_sym(&lib, sym_name.as_bytes())?;
            dispatch.push(fn_ptr);
        }

        Ok(Self {
            _lib: lib,
            dispatch: Arc::new(dispatch),
            create: fn_create,
            destroy: fn_destroy,
        })
    }

    pub fn create_client(
        self: &Arc<Self>,
        config: &HashMap<String, String>,
    ) -> Result<DynSpireClient, String> {
        let buf = serialize_kvmap(config);
        let handle_ptr = unsafe { (self.create)(buf.as_ptr(), buf.len()) };
        if handle_ptr.is_null() {
            return Err("spier create returned null".into());
        }
        Ok(DynSpireClient {
            lib: Arc::clone(self),
            handle: Arc::new(Handle(handle_ptr)),
        })
    }

    pub fn find(name: &str) -> Result<String, String> {
        let filename = format!("lib{name}.so");

        if let Ok(dir) = std::env::var("DYNSPIRE_LIB_DIR") {
            let path = PathBuf::from(&dir).join(&filename);
            if path.exists() {
                return Ok(path.to_string_lossy().into_owned());
            }
            return Err(format!(
                "spier not found: {} (DYNSPIRE_LIB_DIR={dir})",
                path.display()
            ));
        }

        Ok(filename)
    }
}

pub struct DynSpireClient {
    lib: Arc<DynSpireLib>,
    handle: Arc<Handle>,
}

unsafe impl Send for DynSpireClient {}
unsafe impl Sync for DynSpireClient {}

impl DynSpireClient {
    pub fn connect(
        spier_name: &str,
        idl: &'static IdlDescriptor,
        config: &HashMap<String, String>,
    ) -> Result<Self, String> {
        let so_path = DynSpireLib::find(spier_name)?;
        let methods: Vec<MethodConfig> = idl
            .methods
            .iter()
            .map(|&n| MethodConfig { name: n })
            .collect();
        let lib = DynSpireLib::load(&so_path, idl.hash, &methods)?;
        Arc::new(lib).create_client(config)
    }

    pub fn load(
        so_path: &str,
        idl_hash: u64,
        methods: &[MethodConfig],
        config: &HashMap<String, String>,
    ) -> Result<Self, String> {
        let lib = DynSpireLib::load(so_path, idl_hash, methods)?;
        Arc::new(lib).create_client(config)
    }

    pub fn dispatch(
        &self,
        method: usize,
        in_slots: &[u64],
        out_slots: &mut [u64; MAX_OUT_SLOTS],
    ) -> Result<(), String> {
        let dispatch_fn = self
            .lib
            .dispatch
            .get(method)
            .ok_or_else(|| format!("unknown dispatch index: {method}"))?;

        let ret = unsafe {
            dispatch_fn(
                self.handle.0,
                in_slots.as_ptr(),
                in_slots.len(),
                out_slots.as_mut_ptr(),
                MAX_OUT_SLOTS,
            )
        };
        if ret != 0 {
            return Err(format!("dispatch transport error (code {ret})"));
        }
        Ok(())
    }

    pub fn call<R: crate::slots::SlotReceive, A: crate::slots::SlotEncode, Op: SpierOp>(
        &self,
        op: Op,
        args: A,
    ) -> Result<R, String> {
        let mut w = crate::slots::SlotWriter::new();
        A::encode(&args, &mut w);
        let mut out_slots = [0u64; MAX_OUT_SLOTS];
        self.dispatch(op.as_index(), w.as_slice(), &mut out_slots)?;
        crate::slots::read_response::<Result<R, String>>(&out_slots)
    }
}

impl Drop for DynSpireClient {
    fn drop(&mut self) {
        unsafe {
            (self.lib.destroy)(self.handle.0);
        }
    }
}
