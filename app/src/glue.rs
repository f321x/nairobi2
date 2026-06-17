//! Android platform implementation over JNI.
//!
//! The Java side (`io.nairobi.app.LocationBridge`) owns the Android
//! LocationManager, the runtime-permission flow, the navigation hand-off and
//! notifications; this module registers the native callbacks it invokes and
//! forwards [`Platform`] calls into its static methods.
//!
//! Important (the CheckJNI golden rules, copied from ntrack):
//! * The context published by the android-activity glue
//!   (`ndk_context::android_context().context()`) is the **Application**, not
//!   the Activity — passing it where Java expects an `Activity` aborts under
//!   CheckJNI. We therefore never pass a context across JNI: the bridge methods
//!   take only primitives/strings and resolve the live activity on the Java
//!   side (`MainActivity.current()`). The Application context is used here once,
//!   at init, to reach the app class loader.
//! * App classes are invisible to `FindClass` on the native (tokio worker)
//!   threads, so we load `LocationBridge` through the Application context's
//!   class loader instead.
//!
//! The module compiles on every platform (so host `cargo check`/`clippy` cover
//! it) but can only be *constructed* on Android, where `ndk-context` is
//! initialized by the android-activity glue.

use std::ffi::c_void;
use std::sync::RwLock;

use jni::objects::{GlobalRef, JClass, JObject, JValue};
use jni::sys::{jboolean, jdouble};
use jni::{JNIEnv, JavaVM, NativeMethod};
use nairobi_core::geo::LatLng;
use tokio::sync::mpsc;

use crate::platform::{Platform, PlatformEvent};

const BRIDGE_CLASS: &str = "io.nairobi.app.LocationBridge";

/// Sink for events arriving from Java callbacks. One per process. Guarded so a
/// Java callback can't read it mid-swap (e.g. on Activity recreation).
static PLATFORM_TX: RwLock<Option<mpsc::UnboundedSender<PlatformEvent>>> = RwLock::new(None);

/// Install `tx` as the destination for Java→Rust events, replacing any previous
/// one (the engine that owns the platform owns the sink).
fn set_platform_tx(tx: mpsc::UnboundedSender<PlatformEvent>) {
    *PLATFORM_TX.write().unwrap() = Some(tx);
}

/// Clone the current event sink, if any.
fn platform_tx() -> Option<mpsc::UnboundedSender<PlatformEvent>> {
    PLATFORM_TX.read().unwrap().clone()
}

/// The native methods Java's `LocationBridge` calls back into. The JNI
/// signature strings MUST match the Java `static native` declarations exactly.
fn register_bridge_natives(env: &mut JNIEnv, bridge_class: &JClass) -> Result<(), String> {
    env.register_native_methods(
        bridge_class,
        &[
            NativeMethod {
                // static native void nativeOnLocation(double lat, double lng)
                name: "nativeOnLocation".into(),
                sig: "(DD)V".into(),
                fn_ptr: native_on_location as *mut c_void,
            },
            NativeMethod {
                // static native void nativeOnPermission(boolean granted)
                name: "nativeOnPermission".into(),
                sig: "(Z)V".into(),
                fn_ptr: native_on_permission as *mut c_void,
            },
        ],
    )
    .map_err(|e| format!("register natives: {e}"))
}

pub struct AndroidPlatform {
    vm: JavaVM,
    bridge: GlobalRef,
}

impl AndroidPlatform {
    /// Build the platform from the ambient Android context provided by the
    /// android-activity glue, register native methods on the bridge class and
    /// store `tx` as the destination for Java→Rust events.
    pub fn new(tx: mpsc::UnboundedSender<PlatformEvent>) -> Result<Self, String> {
        let ctx = ndk_context::android_context();
        let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }
            .map_err(|e| format!("JavaVM::from_raw: {e}"))?;

        let bridge = {
            let mut env = vm
                .attach_current_thread()
                .map_err(|e| format!("attach: {e}"))?;

            // App classes are invisible to FindClass on native threads; go
            // through the app context's class loader instead. (`context()` is
            // the Application object — fine for getClassLoader.)
            let context_obj = unsafe { JObject::from_raw(ctx.context() as jni::sys::jobject) };
            let loader = env
                .call_method(
                    &context_obj,
                    "getClassLoader",
                    "()Ljava/lang/ClassLoader;",
                    &[],
                )
                .and_then(|v| v.l())
                .map_err(|e| format!("getClassLoader: {e}"))?;
            let class_name = env
                .new_string(BRIDGE_CLASS)
                .map_err(|e| format!("new_string: {e}"))?;
            let bridge_obj = env
                .call_method(
                    &loader,
                    "loadClass",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[JValue::Object(&class_name)],
                )
                .and_then(|v| v.l())
                .map_err(|e| format!("loadClass {BRIDGE_CLASS}: {e}"))?;
            let bridge = env
                .new_global_ref(&bridge_obj)
                .map_err(|e| format!("global ref class: {e}"))?;

            let bridge_class: &JClass = (&bridge_obj).into();
            register_bridge_natives(&mut env, bridge_class)?;
            bridge
        };

        set_platform_tx(tx);
        Ok(Self { vm, bridge })
    }

    /// Attach (if needed) and run `f` with the env and the bridge class. JNI
    /// errors are logged and swallowed: platform calls are fire-and-forget.
    fn with_env<R>(
        &self,
        what: &str,
        f: impl FnOnce(&mut JNIEnv, &JClass) -> jni::errors::Result<R>,
    ) -> Option<R> {
        let mut guard = match self.vm.attach_current_thread() {
            Ok(g) => g,
            Err(e) => {
                log::error!("jni attach failed for {what}: {e}");
                return None;
            }
        };
        let env: &mut JNIEnv = &mut guard;
        let bridge_obj = self.bridge.as_obj();
        let class: &JClass = bridge_obj.into();
        match f(env, class) {
            Ok(r) => Some(r),
            Err(e) => {
                log::error!("jni call {what} failed: {e}");
                if env.exception_check().unwrap_or(false) {
                    let _ = env.exception_describe();
                    let _ = env.exception_clear();
                }
                None
            }
        }
    }
}

impl Platform for AndroidPlatform {
    fn has_location_permission(&self) -> bool {
        self.with_env("hasLocationPermission", |env, class| {
            env.call_static_method(class, "hasLocationPermission", "()Z", &[])
                .and_then(|v| v.z())
        })
        .unwrap_or(false)
    }

    fn request_location_permission(&self) {
        self.with_env("requestLocationPermission", |env, class| {
            env.call_static_method(class, "requestLocationPermission", "()V", &[])
                .map(|_| ())
        });
    }

    fn start_location(&self, interval_ms: u64) {
        self.with_env("startLocation", |env, class| {
            env.call_static_method(
                class,
                "startLocation",
                "(J)V",
                &[JValue::Long(interval_ms as i64)],
            )
            .map(|_| ())
        });
    }

    fn stop_location(&self) {
        self.with_env("stopLocation", |env, class| {
            env.call_static_method(class, "stopLocation", "()V", &[])
                .map(|_| ())
        });
    }

    fn open_nav(&self, lat: f64, lng: f64, label: &str) {
        self.with_env("openNav", |env, class| {
            let jlabel = env.new_string(label)?;
            env.call_static_method(
                class,
                "openNav",
                "(DDLjava/lang/String;)V",
                &[
                    JValue::Double(lat),
                    JValue::Double(lng),
                    JValue::Object(&jlabel),
                ],
            )
            .map(|_| ())
        });
    }

    fn notify(&self, title: &str, body: &str) {
        self.with_env("notify", |env, class| {
            let jtitle = env.new_string(title)?;
            let jbody = env.new_string(body)?;
            env.call_static_method(
                class,
                "notify",
                "(Ljava/lang/String;Ljava/lang/String;)V",
                &[JValue::Object(&jtitle), JValue::Object(&jbody)],
            )
            .map(|_| ())
        });
    }
}

/// `static native void nativeOnLocation(double lat, double lng)` — called by
/// Java on the main looper for every location fix.
extern "system" fn native_on_location(
    _env: JNIEnv,
    _class: JClass,
    lat: jdouble,
    lng: jdouble,
) {
    if let Some(tx) = platform_tx() {
        let _ = tx.send(PlatformEvent::Location(LatLng::new(lat, lng)));
    }
}

/// `static native void nativeOnPermission(boolean)` — result of the runtime
/// permission request.
extern "system" fn native_on_permission(_env: JNIEnv, _class: JClass, granted: jboolean) {
    if let Some(tx) = platform_tx() {
        let _ = tx.send(PlatformEvent::PermissionResult(granted != 0));
    }
}
