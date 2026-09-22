//! Runtime seam for desktop and headless builds.
//!
//! Desktop keeps Tauri's real wry runtime. Gardend uses a deliberately small
//! native facade: typed managed state, path resolution, no-op frontend events,
//! and a shared Tokio runtime. Keeping that facade in this module lets the core
//! services compile unchanged without pulling Tauri's GTK/WebKit closure into
//! the cloud cell.

#[cfg(feature = "desktop")]
pub(crate) type GardenTauriRuntime = tauri::Wry;

#[cfg(all(not(feature = "desktop"), not(feature = "headless")))]
compile_error!("enable at least one of the `desktop` or `headless` features");

#[cfg(feature = "desktop")]
pub(crate) type AppHandle = tauri::AppHandle<GardenTauriRuntime>;

#[cfg(feature = "desktop")]
#[allow(dead_code)]
pub(crate) type App = tauri::App<GardenTauriRuntime>;

#[cfg(feature = "desktop")]
pub(crate) type State<'a, T> = tauri::State<'a, T>;

#[cfg(feature = "desktop")]
pub mod async_runtime {
    pub use tauri::async_runtime::{block_on, set, spawn, spawn_blocking};
}

#[cfg(all(not(feature = "desktop"), feature = "headless"))]
mod native {
    use serde::Serialize;
    use std::{
        any::{Any, TypeId},
        collections::HashMap,
        fmt,
        marker::PhantomData,
        ops::Deref,
        path::PathBuf,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, RwLock,
        },
    };

    type ManagedState = Arc<dyn Any + Send + Sync>;

    struct Inner {
        states: RwLock<HashMap<TypeId, ManagedState>>,
        app_data_dir: PathBuf,
        resource_dir: PathBuf,
    }

    /// Minimal application handle used by Gardend and the headless test
    /// harness. Clones share the same type-indexed state registry.
    #[derive(Clone)]
    pub struct AppHandle {
        inner: Arc<Inner>,
    }

    impl fmt::Debug for AppHandle {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .debug_struct("HeadlessAppHandle")
                .field("app_data_dir", &self.inner.app_data_dir)
                .field("resource_dir", &self.inner.resource_dir)
                .finish_non_exhaustive()
        }
    }

    impl AppHandle {
        /// Register one value for a concrete Rust type. As with Tauri's
        /// `Manager::manage`, an existing value wins and returns `false`.
        pub fn manage<T>(&self, value: T) -> bool
        where
            T: Send + Sync + 'static,
        {
            let mut states = self
                .inner
                .states
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if states.contains_key(&TypeId::of::<T>()) {
                return false;
            }
            states.insert(TypeId::of::<T>(), Arc::new(value));
            true
        }

        pub fn try_state<T>(&self) -> Option<State<'_, T>>
        where
            T: Send + Sync + 'static,
        {
            let state = self
                .inner
                .states
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&TypeId::of::<T>())
                .cloned()?;
            let inner = Arc::downcast::<T>(state)
                .expect("headless managed-state TypeId and concrete type diverged");
            Some(State {
                inner,
                _handle: PhantomData,
            })
        }

        #[track_caller]
        pub fn state<T>(&self) -> State<'_, T>
        where
            T: Send + Sync + 'static,
        {
            self.try_state::<T>()
                .unwrap_or_else(|| panic!("state `{}` is not managed", std::any::type_name::<T>()))
        }

        pub fn path(&self) -> PathResolver {
            PathResolver {
                app_data_dir: self.inner.app_data_dir.clone(),
                resource_dir: self.inner.resource_dir.clone(),
            }
        }

        /// Headless cells have no frontend event bus. These events are desktop
        /// notifications only, so accepting them as a no-op preserves the core
        /// write path without inventing an unseen cloud consumer.
        pub fn emit<S>(&self, _event: &str, _payload: &S) -> Result<(), EventError>
        where
            S: Serialize + ?Sized,
        {
            Ok(())
        }
    }

    /// Owning shell retained by the Gardend process for the lifetime of all
    /// cloned handles.
    pub struct App {
        handle: AppHandle,
    }

    impl App {
        pub fn new() -> Self {
            static NEXT_APP_ID: AtomicU64 = AtomicU64::new(1);

            let app_data_dir = std::env::var_os("GARDEN_HEADLESS_APP_DATA_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    std::env::temp_dir().join(format!(
                        "garden-headless-{}-{}",
                        std::process::id(),
                        NEXT_APP_ID.fetch_add(1, Ordering::Relaxed)
                    ))
                });
            let resource_dir = std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(PathBuf::from))
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(std::env::temp_dir);

            Self {
                handle: AppHandle {
                    inner: Arc::new(Inner {
                        states: RwLock::new(HashMap::new()),
                        app_data_dir,
                        resource_dir,
                    }),
                },
            }
        }

        pub fn handle(&self) -> &AppHandle {
            &self.handle
        }
    }

    impl Default for App {
        fn default() -> Self {
            Self::new()
        }
    }

    pub struct State<'a, T> {
        inner: Arc<T>,
        _handle: PhantomData<&'a AppHandle>,
    }

    impl<T> State<'_, T> {
        pub fn inner(&self) -> &T {
            &self.inner
        }
    }

    impl<T> Deref for State<'_, T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            self.inner()
        }
    }

    impl<T> Clone for State<'_, T> {
        fn clone(&self) -> Self {
            Self {
                inner: Arc::clone(&self.inner),
                _handle: PhantomData,
            }
        }
    }

    pub struct PathResolver {
        app_data_dir: PathBuf,
        resource_dir: PathBuf,
    }

    impl PathResolver {
        pub fn app_data_dir(&self) -> Result<PathBuf, PathError> {
            Ok(self.app_data_dir.clone())
        }

        pub fn resource_dir(&self) -> Result<PathBuf, PathError> {
            Ok(self.resource_dir.clone())
        }
    }

    #[derive(Debug)]
    pub struct PathError;

    impl fmt::Display for PathError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("headless application path is unavailable")
        }
    }

    impl std::error::Error for PathError {}

    #[derive(Debug)]
    pub struct EventError;

    impl fmt::Display for EventError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("headless event emission failed")
        }
    }

    impl std::error::Error for EventError {}

    pub mod async_runtime {
        use std::{future::Future, sync::OnceLock};

        static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

        fn handle() -> &'static tokio::runtime::Handle {
            HANDLE.get_or_init(|| {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("build default headless Tokio runtime");
                let runtime = Box::leak(Box::new(runtime));
                runtime.handle().clone()
            })
        }

        /// Install Gardend's explicitly sized process runtime. This must run
        /// before the first spawn/block_on, matching Tauri's prior contract.
        pub fn set(runtime: tokio::runtime::Handle) {
            assert!(
                HANDLE.set(runtime).is_ok(),
                "headless Tokio runtime was already initialized"
            );
        }

        pub fn block_on<F>(future: F) -> F::Output
        where
            F: Future,
        {
            handle().block_on(future)
        }

        pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
        where
            F: Future + Send + 'static,
            F::Output: Send + 'static,
        {
            handle().spawn(future)
        }

        pub fn spawn_blocking<F, R>(function: F) -> tokio::task::JoinHandle<R>
        where
            F: FnOnce() -> R + Send + 'static,
            R: Send + 'static,
        {
            handle().spawn_blocking(function)
        }
    }
}

#[cfg(all(not(feature = "desktop"), feature = "headless"))]
pub use native::{async_runtime, App, AppHandle, State};

#[cfg(all(test, not(feature = "desktop"), feature = "headless"))]
mod tests {
    use super::{async_runtime, App};

    #[test]
    fn headless_managed_state_is_typed_shared_and_first_writer_wins() {
        let app = App::new();
        let handle = app.handle().clone();

        assert!(handle.manage::<String>("first".into()));
        assert!(!handle.manage::<String>("second".into()));
        assert_eq!(handle.state::<String>().inner().as_str(), "first");
        assert_eq!(app.handle().state::<String>().inner().as_str(), "first");
        assert!(handle.try_state::<u64>().is_none());
    }

    #[test]
    fn headless_async_runtime_runs_spawned_work() {
        let answer = async_runtime::block_on(async {
            async_runtime::spawn(async { 42_u8 })
                .await
                .expect("headless task joins")
        });
        assert_eq!(answer, 42);
    }
}
