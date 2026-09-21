use anyhow::{Context, Result};
use jni::{
    objects::{JObject, JString, JValue},
    sys::jstring,
    JNIEnv,
};
use rowd_core::{
    model::{Entry, Invitation, Manifest},
    storage::Store,
};
use std::path::Path;

// Android owns SAF streams; Rust owns networking, hashing and the sync protocol.
struct AndroidStore<'a, 'b, 'c> {
    env: &'a mut JNIEnv<'b>,
    access: &'c JObject<'b>,
    ignore: rowd_core::ignore::Ignore,
}
impl AndroidStore<'_, '_, '_> {
    fn call(&mut self, name: &str, arguments: &[&str]) -> Result<String> {
        let access = self.access;
        self.env.with_local_frame(16, |env| -> Result<String> {
            let strings: Vec<_> = arguments
                .iter()
                .map(|s| env.new_string(s))
                .collect::<std::result::Result<_, _>>()?;
            let objects: Vec<JObject> = strings.into_iter().map(Into::into).collect();
            let values: Vec<_> = objects.iter().map(JValue::Object).collect();
            let sig = format!(
                "({})Ljava/lang/String;",
                "Ljava/lang/String;".repeat(values.len())
            );
            let result = env.call_method(access, name, sig, &values);
            if env.exception_check()? {
                let exception = env.exception_occurred()?;
                env.exception_clear()?;
                let message = env
                    .call_method(exception, "toString", "()Ljava/lang/String;", &[])?
                    .l()?;
                let text: String = env.get_string(&JString::from(message))?.into();
                anyhow::bail!("{text}");
            }
            let object = result?.l()?;
            Ok(env.get_string(&JString::from(object))?.into())
        })
    }
}
impl Store for AndroidStore<'_, '_, '_> {
    fn excluded(&self, path: &str) -> bool {
        self.ignore.matches(path, false)
    }
    fn scan(&mut self) -> Result<Manifest> {
        Ok(serde_json::from_str(&self.call("scanJson", &[])?)?)
    }
    fn snapshot(&mut self, path: &str, entry: &Entry) -> Result<rowd_core::storage::Snapshot> {
        let staged = self.call("snapshot", &[path, &entry.hash])?;
        let result = rowd_core::storage::snapshot_file(Path::new(&staged));
        let _ = std::fs::remove_file(staged);
        result
    }
    fn install(
        &mut self,
        path: &str,
        expected: Option<&str>,
        entry: &Entry,
        staged: &Path,
    ) -> Result<()> {
        self.call(
            "install",
            &[
                path,
                expected.unwrap_or(""),
                &entry.hash,
                staged.to_str().context("invalid staging path")?,
            ],
        )?;
        Ok(())
    }
}

impl rowd_core::managed::ManagedStore for AndroidStore<'_, '_, '_> {
    fn configure(
        &mut self,
        shares: &[rowd_core::config::ShareConfig],
        removed: &[String],
    ) -> Result<()> {
        self.call(
            "configureShares",
            &[
                &serde_json::to_string(shares)?,
                &serde_json::to_string(removed)?,
            ],
        )?;
        Ok(())
    }
    fn select(&mut self, id: &str) -> Result<std::path::PathBuf> {
        let path = self.call("selectShare", &[id])?;
        self.ignore = rowd_core::ignore::Ignore::parse(&self.call("ignoreText", &[])?);
        Ok(path.into())
    }
    fn known_shares(&mut self) -> Result<Vec<rowd_core::config::ShareConfig>> {
        Ok(serde_json::from_str(&self.call("knownShares", &[])?)?)
    }
    fn pending_share_requests(&mut self) -> Result<Vec<rowd_core::config::ShareRequest>> {
        Ok(serde_json::from_str(
            &self.call("pendingShareRequests", &[])?,
        )?)
    }
    fn acknowledge_share_requests(
        &mut self,
        accepted: &[String],
        rejected: &[String],
        cancelled: &[String],
    ) -> Result<()> {
        self.call(
            "acknowledgeShareRequests",
            &[
                &serde_json::to_string(accepted)?,
                &serde_json::to_string(rejected)?,
                &serde_json::to_string(cancelled)?,
            ],
        )?;
        Ok(())
    }
    fn unlink_requested(&mut self) -> Result<bool> {
        Ok(self.call("unlinkRequested", &[])? == "true")
    }
    fn confirm_unlinked(&mut self) -> Result<()> {
        self.call("confirmUnlinked", &[])?;
        Ok(())
    }
    fn available_shares(&mut self) -> Result<Vec<String>> {
        Ok(serde_json::from_str(&self.call("availableShares", &[])?)?)
    }
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_sync<'local>(
    mut env: JNIEnv<'local>,
    _class: JObject<'local>,
    invitation: JString<'local>,
    device_id: JString<'local>,
    access: JObject<'local>,
) -> jstring {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<String> {
        let json: String = env.get_string(&invitation)?.into();
        let device: String = env.get_string(&device_id)?.into();
        let invite: Invitation = serde_json::from_str(&json)?;
        let mut store = AndroidStore {
            env: &mut env,
            access: &access,
            ignore: rowd_core::ignore::Ignore::default(),
        };
        // One sync worker per process; private app cache is writable on Android.
        std::env::set_var("TMPDIR", store.call("tempDirectory", &[])?);
        let report = rowd_core::managed::client_round(&invite, &device, &mut store)?;
        Ok(serde_json::to_string(&report)?)
    }));
    let output = match result {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => serde_json::json!({"error":format!("{e:#}")}).to_string(),
        Err(_) => {
            serde_json::json!({"error":"Falha interna do Rowd; os backups foram preservados."})
                .to_string()
        }
    };
    env.new_string(output)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}
