//! Writes the Swift bindings: `cargo run -p canvas-mobile --features bindgen --bin uniffi-bindgen -- generate …`
//! (`ios/build-core.sh` runs it).

fn main() {
    uniffi::uniffi_bindgen_main()
}
