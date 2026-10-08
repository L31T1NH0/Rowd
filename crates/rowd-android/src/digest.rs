//! Scoped JNI stream hashing: the hasher lives only for this call, including on cancellation.
use jni::{
    objects::{JByteArray, JObject, JValue},
    sys::jstring,
    JNIEnv,
};
use std::io::{self, Read};

struct JavaReader<'a, 'local> {
    env: &'a mut JNIEnv<'local>,
    input: JObject<'local>,
    output: JObject<'local>,
    control: JObject<'local>,
    buffer: JByteArray<'local>,
    size: u64,
}
impl Read for JavaReader<'_, '_> {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        let read = (|| -> anyhow::Result<usize> {
            // invoke() returns a local reference even for Kotlin Unit. Bound its lifetime.
            let unit = self
                .env
                .call_method(&self.control, "invoke", "()Ljava/lang/Object;", &[])?
                .l()?;
            self.env.delete_local_ref(unit)?;
            let count = self
                .env
                .call_method(
                    &self.input,
                    "read",
                    "([BII)I",
                    &[
                        JValue::Object(&self.buffer),
                        JValue::Int(0),
                        JValue::Int(destination.len() as i32),
                    ],
                )?
                .i()?;
            let unit = self
                .env
                .call_method(&self.control, "invoke", "()Ljava/lang/Object;", &[])?
                .l()?;
            self.env.delete_local_ref(unit)?;
            if count == -1 {
                return Ok(0);
            }
            anyhow::ensure!(
                count > 0 && count as usize <= destination.len(),
                "invalid stream read length"
            );
            self.size += count as u64;
            anyhow::ensure!(
                self.size <= rowd_core::model::MAX_FILE,
                "Arquivo maior que 8 GiB."
            );
            if !self.output.is_null() {
                self.env.call_method(
                    &self.output,
                    "write",
                    "([BII)V",
                    &[
                        JValue::Object(&self.buffer),
                        JValue::Int(0),
                        JValue::Int(count),
                    ],
                )?;
            }
            let bytes = self.env.convert_byte_array(&self.buffer)?;
            destination[..count as usize].copy_from_slice(&bytes[..count as usize]);
            Ok(count as usize)
        })();
        read.map_err(|e| io::Error::other(e.to_string()))
    }
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_ContentDigest_hashStream<'local>(
    mut env: JNIEnv<'local>,
    _class: JObject<'local>,
    input: JObject<'local>,
    output: JObject<'local>,
    control: JObject<'local>,
) -> jstring {
    let result = (|| -> anyhow::Result<String> {
        let buffer = env.new_byte_array(64 * 1024)?;
        let mut reader = JavaReader {
            env: &mut env,
            input,
            output,
            control,
            buffer,
            size: 0,
        };
        let (hash, size) = rowd_core::hash_reader(&mut reader)?;
        Ok(format!("{hash}:{size}"))
    })();
    match result {
        Ok(result) => match env.new_string(result) {
            Ok(result) => result.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        Err(error) => {
            // Keep the original Java exception (cancellation, read/write error, etc.).
            if !env.exception_check().unwrap_or(true) {
                let _ = env.throw_new("java/lang/IllegalStateException", error.to_string());
            }
            std::ptr::null_mut()
        }
    }
}
