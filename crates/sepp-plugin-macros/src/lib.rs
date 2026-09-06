//! `sepp-plugin-macros` — das Attribut `#[tool]` des Plugin-SDKs `sepp-plugin`.
//!
//! Der Autor schreibt eine Funktion; das Makro erzeugt daraus das Aufrufprotokoll des Hosts
//! (ABI 1, siehe `wit/sepp.wit`): die Exports `sepp_alloc`, `sepp_spec` und `sepp_call`, die an
//! `sepp_plugin::__abi` delegieren. Die Exports entstehen **im Crate des Autors**, nicht im SDK —
//! das SDK kennt die Autorfunktion nicht, und der Export von `#[no_mangle]`-Symbolen aus einer
//! Bibliothek in die `cdylib` ist ein Implementierungsdetail von rustc, kein Vertrag.
//!
//! Immer erzeugt (auch nativ, damit ein Autor sein Werkzeug ohne wasm32-Target testen kann): ein
//! verstecktes Modul `__sepp_plugin_export` mit `spec_json()` und `call_json(&[u8])`. Nur unter
//! `target_arch = "wasm32"`: die drei `extern "C"`-Exports.
//!
//! Genau **ein** `#[tool]` je Crate — ABI 1 kennt ein Werkzeug je Modul. Ein zweites erzeugt einen
//! Compiler-Fehler wegen doppelter Definition von `__sepp_plugin_export`.
//!
//! Dieses Crate ist eigenständig (keine sepp-Deps): Proc-Macros laufen auf dem Host, nicht im
//! Modul, und dürfen den Guest-Build nicht belasten.

use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::{FnArg, ItemFn, LitStr, ReturnType, Type};

/// Macht aus einer Funktion `fn name(args: Args, host: &Host) -> Result<ToolResult>` das
/// Werkzeug eines Plugins.
///
/// ```ignore
/// #[sepp_plugin::tool(desc = "Zählt Wörter", name = "wortzaehler", label = "Wortzähler")]
/// fn count(args: Args, host: &Host) -> Result<ToolResult> { … }
/// ```
///
/// - `desc` (Pflicht): die Beschreibung, die das Modell liest.
/// - `name` (optional, Default: Funktionsname): muss `^[A-Za-z0-9_-]{1,64}$` erfüllen — der
///   Host lehnt alles andere beim Laden ab, weil die Anbieter sonst den ganzen Request verwerfen.
/// - `label` (optional, Default: `name`): Anzeigename in der Oberfläche.
///
/// `Args` braucht `serde::Deserialize` und `schemars::JsonSchema`; das Parameter-Schema wird
/// daraus abgeleitet. Fehler und ungültige Argumente werden zu einem `ToolResult` mit
/// `is_error = true` — ein Plugin trappt nie.
#[proc_macro_attribute]
pub fn tool(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    match expand(attr.into(), item.into()) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Die geparsten Attribute von `#[tool(…)]`.
#[derive(Default)]
struct Attrs {
    desc: Option<LitStr>,
    name: Option<LitStr>,
    label: Option<LitStr>,
}

fn parse_attrs(attr: TokenStream) -> syn::Result<Attrs> {
    let mut attrs = Attrs::default();
    if attr.is_empty() {
        return Ok(attrs);
    }
    let parser = syn::meta::parser(|meta| {
        let slot = if meta.path.is_ident("desc") {
            &mut attrs.desc
        } else if meta.path.is_ident("name") {
            &mut attrs.name
        } else if meta.path.is_ident("label") {
            &mut attrs.label
        } else {
            return Err(meta.error("unbekanntes Attribut; erlaubt sind desc, name und label"));
        };
        if slot.is_some() {
            return Err(meta.error("Attribut doppelt angegeben"));
        }
        *slot = Some(meta.value()?.parse()?);
        Ok(())
    });
    parser.parse2(attr)?;
    Ok(attrs)
}

/// Dieselbe Regel wie `sepp_core::is_valid_tool_name` — hier kopiert, weil ein Proc-Macro-Crate
/// keine sepp-Deps zieht. Der Host prüft beim Laden noch einmal.
fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Der eigentliche Umbau — getrennt vom `proc_macro`-Einstieg, damit er sich mit `quote!`-Eingaben
/// testen lässt.
fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let attr_span = attr.span();
    let attrs = parse_attrs(attr)?;
    let func: ItemFn = syn::parse2(item)?;
    let sig = &func.sig;

    let Some(desc) = attrs.desc else {
        return Err(syn::Error::new(
            attr_span,
            "`desc = \"…\"` fehlt — die Beschreibung, die das Modell liest",
        ));
    };
    if sig.asyncness.is_some() {
        return Err(syn::Error::new_spanned(
            sig.asyncness,
            "ein Werkzeug darf nicht `async` sein — das Modul läuft synchron",
        ));
    }
    if !sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &sig.generics,
            "ein Werkzeug darf keine Generics haben",
        ));
    }
    if sig.inputs.len() != 2 {
        return Err(syn::Error::new_spanned(
            &sig.inputs,
            "ein Werkzeug hat genau zwei Parameter: `(args: Args, host: &Host)`",
        ));
    }
    let mut inputs = sig.inputs.iter();
    let arg_ty = match inputs.next() {
        Some(FnArg::Typed(pat)) => pat.ty.clone(),
        other => {
            return Err(syn::Error::new_spanned(
                other,
                "der erste Parameter sind die Argumente: `args: Args`",
            ))
        }
    };
    match inputs.next() {
        Some(FnArg::Typed(pat)) if matches!(*pat.ty, Type::Reference(_)) => {}
        other => {
            return Err(syn::Error::new_spanned(
                other,
                "der zweite Parameter muss `host: &Host` sein",
            ))
        }
    }
    if matches!(sig.output, ReturnType::Default) {
        return Err(syn::Error::new_spanned(
            sig,
            "ein Werkzeug gibt `Result<ToolResult>` zurück",
        ));
    }

    let fn_ident = &sig.ident;
    let name = attrs
        .name
        .as_ref()
        .map(LitStr::value)
        .unwrap_or_else(|| fn_ident.to_string());
    if !is_valid_tool_name(&name) {
        let span = attrs
            .name
            .as_ref()
            .map(Spanned::span)
            .unwrap_or_else(|| fn_ident.span());
        return Err(syn::Error::new(
            span,
            format!(
                "Werkzeugname {name:?} ist unzulässig — erlaubt sind 1 bis 64 Zeichen aus \
                 A-Z, a-z, 0-9, _ und - (sonst `name = \"…\"` setzen)"
            ),
        ));
    }
    let label = attrs
        .label
        .as_ref()
        .map(LitStr::value)
        .unwrap_or_else(|| name.clone());

    Ok(quote! {
        #func

        #[doc(hidden)]
        pub mod __sepp_plugin_export {
            #[allow(unused_imports)]
            use super::*;

            /// Die Werkzeugbeschreibung als JSON — was `sepp_spec` liefert.
            pub fn spec_json() -> ::std::string::String {
                ::sepp_plugin::__abi::spec_json::<#arg_ty>(#name, #label, #desc)
            }

            /// Ein Aufruf: Argument-JSON hinein, ToolResult-JSON hinaus — was `sepp_call` tut.
            pub fn call_json(raw: &[u8]) -> ::std::string::String {
                ::sepp_plugin::__abi::call_json::<#arg_ty, _>(#name, raw, super::#fn_ident)
            }
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn sepp_alloc(n: i32) -> i32 {
            ::sepp_plugin::__abi::alloc(n)
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn sepp_spec() -> i64 {
            ::sepp_plugin::__abi::emit(__sepp_plugin_export::spec_json())
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn sepp_call(ptr: i32, len: i32) -> i64 {
            // SAFETY: `ptr`/`len` beschreiben den Puffer, den der Host über `sepp_alloc` belegt
            // und mit den Argumenten gefüllt hat — so verlangt es das ABI.
            let raw = unsafe { ::sepp_plugin::__abi::input(ptr, len) };
            ::sepp_plugin::__abi::emit(__sepp_plugin_export::call_json(raw))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_fn() -> TokenStream {
        quote! {
            fn zaehlen(args: Args, host: &Host) -> Result<ToolResult> { todo!() }
        }
    }

    fn err_text(attr: TokenStream, item: TokenStream) -> String {
        expand(attr, item).expect_err("Fehler erwartet").to_string()
    }

    #[test]
    fn valid_input_expands_to_exports_and_hidden_module() {
        let out = expand(quote!(desc = "Zählt"), ok_fn()).unwrap().to_string();
        assert!(out.contains("__sepp_plugin_export"), "{out}");
        assert!(out.contains("sepp_alloc"), "{out}");
        assert!(out.contains("sepp_spec"), "{out}");
        assert!(out.contains("sepp_call"), "{out}");
        assert!(out.contains("spec_json"), "{out}");
        assert!(out.contains("\"Zählt\""), "{out}");
        // Ohne `name`/`label` heißt das Werkzeug wie die Funktion, das Label wie das Werkzeug.
        assert!(
            out.contains("\"zaehlen\" , \"zaehlen\" , \"Zählt\""),
            "{out}"
        );
        // Die Originalfunktion bleibt erhalten.
        assert!(out.contains("fn zaehlen"), "{out}");
    }

    #[test]
    fn name_and_label_override_the_defaults() {
        let out = expand(
            quote!(desc = "d", name = "wort-zaehler", label = "Wortzähler"),
            ok_fn(),
        )
        .unwrap()
        .to_string();
        assert!(
            out.contains("\"wort-zaehler\" , \"Wortzähler\" , \"d\""),
            "{out}"
        );
    }

    #[test]
    fn missing_desc_is_an_error() {
        assert!(err_text(quote!(), ok_fn()).contains("desc"));
        assert!(err_text(quote!(name = "x"), ok_fn()).contains("desc"));
    }

    #[test]
    fn unknown_or_duplicate_attributes_are_errors() {
        assert!(err_text(quote!(desc = "d", foo = "x"), ok_fn()).contains("unbekanntes Attribut"));
        assert!(err_text(quote!(desc = "d", desc = "e"), ok_fn()).contains("doppelt"));
    }

    #[test]
    fn invalid_tool_names_are_rejected_at_compile_time() {
        let e = err_text(quote!(desc = "d", name = "rp:pdf"), ok_fn());
        assert!(e.contains("unzulässig"), "{e}");
        // Auch ein Funktionsname, der als Default dient, unterliegt der Regel — hier zu lang.
        let long = quote::format_ident!("{}", "x".repeat(65));
        let e = err_text(
            quote!(desc = "d"),
            quote! { fn #long(args: Args, host: &Host) -> Result<ToolResult> { todo!() } },
        );
        assert!(e.contains("unzulässig"), "{e}");
    }

    #[test]
    fn wrong_signatures_are_explained() {
        let e = err_text(
            quote!(desc = "d"),
            quote! { fn f(args: Args) -> Result<ToolResult> { todo!() } },
        );
        assert!(e.contains("zwei Parameter"), "{e}");

        let e = err_text(
            quote!(desc = "d"),
            quote! { async fn f(args: Args, host: &Host) -> Result<ToolResult> { todo!() } },
        );
        assert!(e.contains("async"), "{e}");

        let e = err_text(
            quote!(desc = "d"),
            quote! { fn f(args: Args, host: Host) -> Result<ToolResult> { todo!() } },
        );
        assert!(e.contains("&Host"), "{e}");

        let e = err_text(
            quote!(desc = "d"),
            quote! { fn f(args: Args, host: &Host) { } },
        );
        assert!(e.contains("Result<ToolResult>"), "{e}");

        let e = err_text(
            quote!(desc = "d"),
            quote! { fn f<T>(args: T, host: &Host) -> Result<ToolResult> { todo!() } },
        );
        assert!(e.contains("Generics"), "{e}");
    }
}
