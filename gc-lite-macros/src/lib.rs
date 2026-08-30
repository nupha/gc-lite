// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    Data, DeriveInput, Fields, GenericParam, Ident, Index, LitInt, Path, Result as SynResult,
    Token, Type,
    parse::{Parse, ParseStream},
    parse_macro_input, parse_quote,
};

struct TypeTableEntry {
    ty: Type,
    drop_pass: Option<u8>,
}

struct TypeTableInput {
    crate_path: Path,
    entries: Vec<TypeTableEntry>,
}

impl Parse for TypeTableInput {
    fn parse(input: ParseStream<'_>) -> SynResult<Self> {
        let key_ident: Ident = input.parse()?;
        if key_ident != "crate_path" {
            return Err(syn::Error::new_spanned(
                key_ident,
                "expected `crate_path = <path>`",
            ));
        }
        input.parse::<Token![=]>()?;
        let crate_path: Path = input.parse()?;
        input.parse::<Token![;]>()?;

        let mut entries = Vec::new();

        while !input.is_empty() {
            let ty: Type = input.parse()?;

            let mut drop_pass: Option<u8> = None;

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
                let ident: Ident = input.parse()?;
                if ident != "drop_pass" {
                    return Err(syn::Error::new_spanned(
                        ident,
                        "expected `drop_pass = N` or omit for default 0",
                    ));
                }
                input.parse::<Token![=]>()?;
                let lit: LitInt = input.parse()?;
                let v = lit.base10_parse::<u8>()?;
                if v > 3 {
                    return Err(syn::Error::new_spanned(
                        lit,
                        "drop_pass must be 0, 1, 2 or 3",
                    ));
                }
                drop_pass = Some(v);
            }

            input.parse::<Token![;]>()?;

            entries.push(TypeTableEntry { ty, drop_pass });
        }

        Ok(TypeTableInput {
            crate_path,
            entries,
        })
    }
}

/// Function-like proc-macro used internally by gc-lite's `gc_type_register!` wrapper.
///
/// Do not call this directly unless you know what you are doing.
#[proc_macro]
pub fn gc_type_table_internal(input: TokenStream) -> TokenStream {
    let TypeTableInput {
        crate_path,
        entries,
    } = parse_macro_input!(input as TypeTableInput);

    let count = entries.len();
    if count > u8::MAX as usize + 1 {
        return syn::Error::new_spanned(
            crate_path,
            "too many GC types: maximum 256 entries are supported",
        )
        .into_compile_error()
        .into();
    }

    let mut tys = Vec::with_capacity(count);
    let mut passes = Vec::with_capacity(count);
    let mut ids = Vec::with_capacity(count);

    for (idx, entry) in entries.into_iter().enumerate() {
        let ty = entry.ty;
        let pass = entry.drop_pass.unwrap_or(0);
        let id = idx as u8;

        tys.push(ty);
        passes.push(pass);
        ids.push(id);
    }

    let mut drop_passes = passes.clone();
    drop_passes.sort_unstable();
    drop_passes.dedup();

    let expanded = quote! {
        pub const GC_TYPE_REGISTRY: #crate_path::gctype::GcTypeRegistry = #crate_path::gctype::GcTypeRegistry {
            type_info_list: &[
                #(
                    #crate_path::GcTypeInfo {
                        size: ::core::mem::size_of::<#tys>(),
                        payload_offset: #crate_path::gctype::payload_offset_of::<#tys>(),
                        layout_size: #crate_path::gctype::layout_size_of::<#tys>(),
                        layout_align: #crate_path::gctype::layout_align_of::<#tys>(),
                        trace_fn: #crate_path::gctype_trace::<#tys>,
                        drop_fn: {
                            if ::core::mem::needs_drop::<#tys>() {
                                Some(#crate_path::gctype_drop::<#tys>)
                            } else {
                                None
                            }
                        },
                        drop_pass: #passes,
                    },
                )*
            ],
            drop_passes: &[
                #(
                    #drop_passes
                ),*
            ],
        };

        #(
        impl #crate_path::GcNode for #tys {
            const GC_TYPE_ID: u8 = #ids;

            #[inline(always)]
            fn gc_ref(&self) -> #crate_path::GcRef<Self> {
                unsafe { #crate_path::GcRef::<Self>::from_ref_unchecked(self)  }
            }
        }

        impl #tys {
            pub fn alloc_node(
                heap: &mut #crate_path::GcHeap,
                scope: #crate_path::GcPartitionId,
                payload: #tys,
            ) -> Result<#crate_path::GcRef<#tys>, (#crate_path::GcError, #tys)> {
                unsafe { heap.alloc_raw(scope, payload) }
            }
        }
        )*
    };

    expanded.into()
}

// ─────────────────────────────────────────────────────────────────────────────
// #[derive(GcTrace)]
// ─────────────────────────────────────────────────────────────────────────────
//
// The derive makes one design decision above all: it does NOT decide which
// fields need tracing. It unconditionally emits a `GcTrace::trace` call for
// every field and lets trait resolution give each call its meaning:
//
// - a no-op impl (`bool`, `u32`, `String`, ...) makes the call free,
// - a real impl (`GcRef<T>`, `Vec<T>`, `Option<T>`, ...) does the tracing,
// - a missing impl is a COMPILE ERROR pointing at the field, so a payload
//   holding untraced GC references cannot be built silently. Opting out of
//   tracing a field is always an explicit, reviewable `#[gc(skip)]`.
//
// Supported attributes (on struct fields and enum variant fields):
// - `#[gc(skip)]`        — do not emit any call for this field
// - `#[gc(with = "path")]` — call `path(&field, ctx)` instead of the trait
//
/// Derive macro for gc-lite's [`GcTrace`] trait (`gc_lite::GcTrace`).
///
/// Emits one `GcTrace::trace` call per field (or per enum-variant field), so
/// every field is either traced or explicitly opted out — a field whose type
/// does not implement `GcTrace` is a compile error, never a silent gap.
///
/// See `gc_lite::GcTrace` for usage examples. Runnable examples and
/// compile-fail tests live in the `gc-lite` crate docs (the macro crate
/// cannot link `gc_lite` for doctests).
#[proc_macro_derive(GcTrace, attributes(gc))]
pub fn derive_gc_trace(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_gc_trace(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// What to emit for one field.
#[derive(Clone)]
enum FieldKind {
    /// default: `GcTrace::trace(&field, ctx)`
    Trace,
    /// `#[gc(skip)]`: emit nothing
    Skip,
    /// `#[gc(with = "path")]`: `path(&field, ctx)`
    With(Path),
}

/// At most one `#[gc(...)]` attribute per field; unknown keys are rejected so
/// typos cannot silently degrade to default tracing.
fn field_kind(attrs: &[syn::Attribute]) -> SynResult<FieldKind> {
    let mut kind: Option<FieldKind> = None;
    for attr in attrs.iter().filter(|a| a.path().is_ident("gc")) {
        attr.parse_nested_meta(|meta| {
            let new_kind = if meta.path.is_ident("skip") {
                FieldKind::Skip
            } else if meta.path.is_ident("with") {
                let lit: syn::LitStr = meta.value()?.parse()?;
                FieldKind::With(lit.parse()?)
            } else {
                return Err(meta.error(
                    "unknown `gc` attribute; expected `#[gc(skip)]` or `#[gc(with = \"path\")]`",
                ));
            };
            if kind.is_some() {
                return Err(meta.error("duplicate `gc` attribute on the same field"));
            }
            kind = Some(new_kind);
            Ok(())
        })?;
    }
    Ok(kind.unwrap_or(FieldKind::Trace))
}

fn named_field_stmts(
    fields: &syn::punctuated::Punctuated<syn::Field, Token![,]>,
) -> SynResult<TokenStream2> {
    let mut stmts = TokenStream2::new();
    for f in fields {
        let ident = f.ident.as_ref().expect("named field has ident");
        match field_kind(&f.attrs)? {
            FieldKind::Skip => {}
            FieldKind::Trace => stmts.extend(quote! {
                ::gc_lite::GcTrace::trace(&self.#ident, __gc_trace_ctx);
            }),
            FieldKind::With(p) => stmts.extend(quote! {
                #p(&self.#ident, __gc_trace_ctx);
            }),
        }
    }
    Ok(stmts)
}

fn unnamed_field_stmts(
    fields: &syn::punctuated::Punctuated<syn::Field, Token![,]>,
) -> SynResult<TokenStream2> {
    let mut stmts = TokenStream2::new();
    for (i, f) in fields.iter().enumerate() {
        let idx = Index::from(i);
        match field_kind(&f.attrs)? {
            FieldKind::Skip => {}
            FieldKind::Trace => stmts.extend(quote! {
                ::gc_lite::GcTrace::trace(&self.#idx, __gc_trace_ctx);
            }),
            FieldKind::With(p) => stmts.extend(quote! {
                #p(&self.#idx, __gc_trace_ctx);
            }),
        }
    }
    Ok(stmts)
}

/// One match arm per enum variant. Skipped fields are bound as `_` so the
/// generated pattern never triggers unused-binding warnings.
fn variant_arm(v: &syn::Variant) -> SynResult<TokenStream2> {
    let vname = &v.ident;
    let arm = match &v.fields {
        Fields::Named(fields) => {
            let mut pattern = TokenStream2::new();
            let mut stmts = TokenStream2::new();
            for f in &fields.named {
                let ident = f.ident.as_ref().expect("named field has ident");
                match field_kind(&f.attrs)? {
                    FieldKind::Skip => pattern.extend(quote! { #ident: _, }),
                    FieldKind::Trace => {
                        pattern.extend(quote! { #ident, });
                        stmts.extend(quote! {
                            ::gc_lite::GcTrace::trace(#ident, __gc_trace_ctx);
                        });
                    }
                    FieldKind::With(p) => {
                        pattern.extend(quote! { #ident, });
                        stmts.extend(quote! { #p(#ident, __gc_trace_ctx); });
                    }
                }
            }
            quote! { Self::#vname { #pattern } => { #stmts } }
        }
        Fields::Unnamed(fields) => {
            let mut pattern = TokenStream2::new();
            let mut stmts = TokenStream2::new();
            for (i, f) in fields.unnamed.iter().enumerate() {
                let binding = format_ident!("__field_{}", i);
                match field_kind(&f.attrs)? {
                    FieldKind::Skip => pattern.extend(quote! { _, }),
                    FieldKind::Trace => {
                        pattern.extend(quote! { #binding, });
                        stmts.extend(quote! {
                            ::gc_lite::GcTrace::trace(#binding, __gc_trace_ctx);
                        });
                    }
                    FieldKind::With(p) => {
                        pattern.extend(quote! { #binding, });
                        stmts.extend(quote! { #p(#binding, __gc_trace_ctx); });
                    }
                }
            }
            quote! { Self::#vname(#pattern) => { #stmts } }
        }
        Fields::Unit => quote! { Self::#vname => {} },
    };
    Ok(arm)
}

fn expand_gc_trace(input: &DeriveInput) -> SynResult<TokenStream2> {
    let name = &input.ident;

    if matches!(input.data, Data::Union(_)) {
        return Err(syn::Error::new(
            name.span(),
            "GcTrace cannot be derived for unions",
        ));
    }

    // Add `GcTrace` to every type parameter: `struct Wrap<T>` derives to
    // `impl<T: GcTrace> GcTrace for Wrap<T>`. (`GcTrace: 'static`, so the
    // compiler also requires the self type to be `'static` — payloads are
    // GC-heap resident, which is exactly the intent.)
    let mut generics = input.generics.clone();
    for param in generics.params.iter_mut() {
        if let GenericParam::Type(tp) = param {
            tp.bounds.push(parse_quote!(::gc_lite::GcTrace));
        }
    }
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let body = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => named_field_stmts(&fields.named)?,
            Fields::Unnamed(fields) => unnamed_field_stmts(&fields.unnamed)?,
            Fields::Unit => TokenStream2::new(),
        },
        Data::Enum(data) => {
            let arms = data
                .variants
                .iter()
                .map(variant_arm)
                .collect::<SynResult<Vec<_>>>()?;
            quote! { match self { #(#arms)* } }
        }
        Data::Union(_) => unreachable!("rejected above"),
    };

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::gc_lite::GcTrace for #name #ty_generics #where_clause {
            #[inline]
            fn trace(&self, __gc_trace_ctx: &mut ::gc_lite::GcTraceCtx) {
                #body
            }
        }
    })
}

#[cfg(test)]
mod derive_tests {
    use super::expand_gc_trace;
    use quote::{ToTokens, quote};
    use syn::DeriveInput;

    fn expand(src: &str) -> String {
        let input: DeriveInput = syn::parse_str(src).unwrap();
        expand_gc_trace(&input).unwrap().to_string()
    }

    fn trace_call(field_access: proc_macro2::TokenStream) -> String {
        quote! { ::gc_lite::GcTrace::trace(#field_access, __gc_trace_ctx); }
            .to_token_stream()
            .to_string()
    }

    #[test]
    fn named_struct_traces_every_field() {
        let out = expand("struct S { a: u32, b: Vec<u8>, c: String }");
        for f in ["a", "b", "c"] {
            let ident = syn::Ident::new(f, proc_macro2::Span::call_site());
            let call = trace_call(quote! { &self.#ident });
            assert!(out.contains(&call), "missing `{call}` in:\n{out}");
        }
    }

    #[test]
    fn skip_field_emits_no_call() {
        let out = expand(r#"struct S { a: u32, #[gc(skip)] raw: *mut u8 }"#);
        assert!(out.contains(&trace_call(quote! { &self.a })));
        assert!(!out.contains(&trace_call(quote! { &self.raw })));
    }

    #[test]
    fn with_attribute_calls_given_path() {
        let out = expand(r#"struct S { #[gc(with = "my::trace_it")] items: Vec<u8> }"#);
        let expected = quote! { my::trace_it(&self.items, __gc_trace_ctx); }.to_string();
        assert!(out.contains(&expected), "missing `{expected}` in:\n{out}");
    }

    #[test]
    fn enum_generates_variant_match() {
        let out = expand("enum E { Leaf(u32), Node { l: u8, r: Option<u8> }, Empty }");
        assert!(out.contains("match self"));
        assert!(out.contains(&quote! { Self::Leaf(__field_0,) }.to_string()));
        assert!(out.contains(&quote! { Self::Node { l, r, } }.to_string()));
        assert!(out.contains(&quote! { Self::Empty => {} }.to_string()));
        // 1 (Leaf) + 2 (Node) = 3 traced bindings, 0 for Empty
        let trace = quote! { ::gc_lite::GcTrace::trace }.to_string();
        assert_eq!(out.matches(&trace).count(), 3);
    }

    #[test]
    fn generic_struct_gets_gc_trace_bound() {
        let out = expand("struct Wrap<T> { inner: Vec<T> }");
        assert!(out.contains(&quote! { impl<T: ::gc_lite::GcTrace> }.to_string()));
        assert!(out.contains(&quote! { for Wrap<T> }.to_string()));
    }

    #[test]
    fn unit_struct_generates_empty_body() {
        let out = expand("struct Unit;");
        assert!(out.contains(&quote! { for Unit }.to_string()));
        let trace = quote! { ::gc_lite::GcTrace::trace }.to_string();
        assert_eq!(out.matches(&trace).count(), 0);
    }

    #[test]
    fn unknown_gc_attr_is_rejected() {
        let input: DeriveInput = syn::parse_str("struct S { #[gc(nope)] a: u32 }").unwrap();
        let err = expand_gc_trace(&input).unwrap_err().to_string();
        assert!(err.contains("unknown `gc` attribute"), "{err}");
    }

    #[test]
    fn duplicate_gc_attr_is_rejected() {
        let input: DeriveInput =
            syn::parse_str("struct S { #[gc(skip)] #[gc(skip)] a: u32 }").unwrap();
        let err = expand_gc_trace(&input).unwrap_err().to_string();
        assert!(err.contains("duplicate `gc` attribute"), "{err}");
    }
}
