#![allow(dead_code)] // Refactor WIP

use std::path::PathBuf;

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use spin_app::{
    locked::{LockedApp, LockedComponentSource},
    AppComponent, Loader,
};
use spin_core::StoreBuilder;
use tokio::fs;
use wit_parser::PackageName;

use crate::parse_file_url;

pub struct TriggerLoader {
    working_dir: PathBuf,
    allow_transient_write: bool,
}

impl TriggerLoader {
    pub fn new(working_dir: impl Into<PathBuf>, allow_transient_write: bool) -> Self {
        Self {
            working_dir: working_dir.into(),
            allow_transient_write,
        }
    }
}

#[async_trait]
impl Loader for TriggerLoader {
    async fn load_app(&self, url: &str) -> Result<LockedApp> {
        let path = parse_file_url(url)?;
        let contents =
            std::fs::read(&path).with_context(|| format!("failed to read manifest at {path:?}"))?;
        let app =
            serde_json::from_slice(&contents).context("failed to parse app lock file JSON")?;
        Ok(app)
    }

    async fn load_component(
        &self,
        engine: &spin_core::wasmtime::Engine,
        source: &LockedComponentSource,
    ) -> Result<spin_core::Component> {
        let source = source
            .content
            .source
            .as_ref()
            .context("LockedComponentSource missing source field")?;
        let path = parse_file_url(source)?;
        let bytes = fs::read(&path).await.with_context(|| {
            format!(
                "failed to read component source from disk at path '{}'",
                path.display()
            )
        })?;
        let component = spin_componentize::componentize_if_necessary(&bytes)?;
        let was_already_component = matches!(component, std::borrow::Cow::Borrowed(_));
        if was_already_component {
            terminal::warn!(
                "Spin component at path {} is a WebAssembly component instead of a \
                WebAssembly module. Use of the WebAssembly component model is an experimental feature.",
                path.display()
            )
        }
        let component = adapt_old_worlds_to_new(&component)?;
        spin_core::Component::new(engine, component.as_ref())
            .with_context(|| format!("loading module {path:?}"))
    }

    async fn load_module(
        &self,
        engine: &spin_core::wasmtime::Engine,
        source: &LockedComponentSource,
    ) -> Result<spin_core::Module> {
        let source = source
            .content
            .source
            .as_ref()
            .context("LockedComponentSource missing source field")?;
        let path = parse_file_url(source)?;
        spin_core::Module::from_file(engine, &path)
            .with_context(|| format!("loading module {path:?}"))
    }

    async fn mount_files(
        &self,
        store_builder: &mut StoreBuilder,
        component: &AppComponent,
    ) -> Result<()> {
        for content_dir in component.files() {
            let source_uri = content_dir
                .content
                .source
                .as_deref()
                .with_context(|| format!("Missing 'source' on files mount {content_dir:?}"))?;
            let source_path = self.working_dir.join(parse_file_url(source_uri)?);
            ensure!(
                source_path.is_dir(),
                "TriggerLoader only supports directory mounts; {source_path:?} is not a directory"
            );
            let guest_path = content_dir.path.clone();
            if self.allow_transient_write {
                store_builder.read_write_preopened_dir(source_path, guest_path)?;
            } else {
                store_builder.read_only_preopened_dir(source_path, guest_path)?;
            }
        }
        Ok(())
    }
}

fn adapt_old_worlds_to_new(component: &[u8]) -> anyhow::Result<std::borrow::Cow<[u8]>> {
    let mut resolve = wit_parser::Resolve::new();
    const SPIN_WIT_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/wit");
    resolve.push_dir(&std::path::Path::new(SPIN_WIT_PATH))?;
    let pkg = resolve
        .package_names
        .get(&PackageName {
            namespace: "fermyon".into(),
            name: "spin".into(),
            version: None,
        })
        .unwrap();
    let spin_world = resolve.select_world(*pkg, Some("platform"))?;
    const WASI_WIT_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/wasi");
    resolve.push_dir(&std::path::Path::new(WASI_WIT_PATH))?;
    let pkg = resolve
        .package_names
        .get(&PackageName {
            namespace: "wasmtime".into(),
            name: "wasi".into(),
            version: None,
        })
        .unwrap();
    let wasi_world = resolve.select_world(*pkg, Some("preview1-adapter-reactor"))?;
    resolve.merge_worlds(wasi_world, spin_world)?;
    // We assume `component` is a valid component and so the only failure possible from `targets`
    // is if the component does not conform to the world
    if wit_component::targets(&resolve, spin_world, component).is_ok() {
        return Ok(std::borrow::Cow::Borrowed(component));
    }

    // Now we compose the incoming component with an adapter component
    // The adapter component exports the Spin 1.5 world and imports the Spin 2.0 world
    // The exports of the adapter fill the incoming component's imports leaving a component
    // that is 2.0 compatible
    todo!()
}
