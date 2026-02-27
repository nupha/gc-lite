// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Ident, LitInt, Path, Result as SynResult, Token, Type,
    parse::{Parse, ParseStream},
    parse_macro_input,
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
                        size: ::core::mem::size_of::<#tys>() as u32,
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
