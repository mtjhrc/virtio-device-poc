// Copyright 2018 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "128"]

extern crate proc_macro;

use proc_macro2::Ident;
use proc_macro2::TokenStream;
use quote::{quote, ToTokens};
use syn::parse_macro_input;
use syn::Data;
use syn::DeriveInput;
use syn::Field;
use syn::Fields;
use syn::Index;
use syn::Member;
use syn::Variant;

#[cfg(test)]
mod tests;

// The method for packing an enum into a u64 is as follows:
// 1) Reserve the lowest "ceil(log_2(x))" bits where x is the number of enum variants.
// 2) Store the enum variant's index (0-based index based on order in the enum definition) in
//    reserved bits.
// 3) If there is data in the enum variant, store the data in remaining bits.
// The method for unpacking is as follows
// 1) Mask the raw token to just the reserved bits
// 2) Match the reserved bits to the enum variant token.
// 3) If the indicated enum variant had data, extract it from the unreserved bits.

// Calculates the number of bits needed to store the variant index. Essentially the log base 2
// of the number of variants, rounded up.
fn variant_bits(variants: &[Variant]) -> u32 {
    if variants.is_empty() {
        // The degenerate case of no variants.
        0
    } else {
        variants.len().next_power_of_two().trailing_zeros()
    }
}

// Name of the field if it has one, otherwise 0 assuming this is the zeroth
// field of a tuple variant.
fn field_member(field: &Field) -> Member {
    match &field.ident {
        Some(name) => Member::Named(name.clone()),
        None => Member::Unnamed(Index::from(0)),
    }
}


// Generates const block which evaluates to the number of bits this enum needs for storing
// information. Also checks if the inner data doesn't use too many bits, if it does it generate a
// panic! (The panic is in const context so it will cause a compile time failure)
fn generate_used_bits_const(
    variants: &[Variant],
    max_allowed_bits: u32,
) -> TokenStream {
    let num_variants = variants.len();
    let variant_bits = variant_bits(variants);

    let used_bits_expressions = variants
        .iter()
        .flat_map(|variant| variant.fields.iter().take(1))
        .map(move |field| {
            let ty = &field.ty;
            let typename = ty.to_token_stream().to_string();
            quote! {
                if <#ty as EventToken>::USED_BITS <= #max_allowed_bits {
                   <#ty as EventToken>::USED_BITS
                } else {
                    panic!(concat!("Cannot derive EventToken: inner type `", #typename, "` is too big (see EventToken::USED_BITS impl for that type). NOTE: Because this enum has ", #num_variants, " variants, the contained values have to be at most ", #max_allowed_bits, " bits."));
                }
            }
        });

    quote! {
        const {
            const fn maximum(nums: &[u32]) -> u32 {
                let mut max = 0;
                let mut i = 0;
                while i < nums.len() {
                    if nums[i] > max {
                        max = nums[i];
                    }
                    i += 1;
                }
                max
            }
            maximum(&[#(#used_bits_expressions),*]) + #variant_bits
        }
    }
}

// Generates the function body for `as_raw_token`.
fn generate_as_raw_token(enum_name: &Ident, variants: &[Variant]) -> TokenStream {
    let variant_bits = variant_bits(variants);

    // Each iteration corresponds to one variant's match arm.
    let cases = variants.iter().enumerate().map(|(index, variant)| {
        let variant_name = &variant.ident;
        let index = index as u64;

        // The capture string is for everything between the variant identifier and the `=>` in
        // the match arm: the variant's data capture.
        let capture = variant.fields.iter().next().map(|field| {
            let member = field_member(field);
            quote!({ #member: data })
        });

        // The modifier string ORs the variant index with extra bits from the variant data
        // field.
        let modifier = match variant.fields {
            Fields::Named(_) | Fields::Unnamed(_) => Some(quote! {
                | (EventToken::as_raw_token(&data) << #variant_bits)
            }),
            Fields::Unit => None,
        };

        // Assembly of the match arm.
        quote! {
            #enum_name::#variant_name #capture => #index #modifier
        }
    });

    quote! {
        match *self {
            #(
                #cases,
            )*
        }
    }
}

// Generates the function body for `from_raw_token`.
fn generate_from_raw_token(enum_name: &Ident, variants: &[Variant]) -> TokenStream {
    let variant_bits = variant_bits(variants);
    let variant_mask = ((1 << variant_bits) - 1) as u64;

    // Each iteration corresponds to one variant's match arm.
    let cases = variants.iter().enumerate().map(|(index, variant)| {
        let variant_name = &variant.ident;
        let index = index as u64;

        // The data string is for extracting the enum variant's data bits out of the raw token
        // data, which includes both variant index and data bits.
        let data = variant.fields.iter().next().map(|field| {
            let member = field_member(field);
            let ty = &field.ty;
            quote!({ #member: (<#ty as EventToken>::from_raw_token(data as u64 >> #variant_bits))})
        });

        // Assembly of the match arm.
        quote! {
            #index => #enum_name::#variant_name #data
        }
    });

    quote! {
        // The match expression only matches the bits for the variant index.
        match data & #variant_mask {
            #(
                #cases,
            )*
            _ => unreachable!(),
        }
    }
}

fn event_token_inner(input: DeriveInput) -> TokenStream {
    let variants: Vec<Variant> = match input.data {
        Data::Enum(data) => data.variants.into_iter().collect(),
        Data::Struct(_) | Data::Union(_) => panic!("input must be an enum"),
    };

    for variant in &variants {
        assert!(variant.fields.iter().count() <= 1);
    }

    let variant_bits = variant_bits(&variants);
    // Maximum number of bits which can be used by the inner types inside the enum
    let max_allowed_bits = u64::BITS - variant_bits;

    let enum_name = input.ident;
    let as_raw_token = generate_as_raw_token(&enum_name, &variants);
    let from_raw_token = generate_from_raw_token(&enum_name, &variants);
    let used_bits_const = generate_used_bits_const(&variants, max_allowed_bits);

    quote! {
        impl EventToken for #enum_name {
            const USED_BITS: u32 = { #used_bits_const };

            fn as_raw_token(&self) -> u64 {
                #as_raw_token
            }

            fn from_raw_token(data: u64) -> Self {
                #from_raw_token
            }
        }

        // Force USED_BITS to be evaluated - it's evaluation can cause compile time failure when
        // EventToken was derived for an enum which cannot be packed into an u64
        const _: () = {
            let _ = <#enum_name as EventToken>::USED_BITS;
        };
    }
}

/// Implements the EventToken trait for a given `enum`.
///
/// There are limitations on what `enum`s this custom derive will work on:
///
/// * Each variant must be a unit variant (no data), or have a single (un)named data field.
/// * If a variant has data, the data must also implement EventToken trait
///   (this is already implemented for basic types e.g. u32)
/// * If a variant data's type takes up too many bits, this derive will fail:
///     - an enum with 2 variants can have at most 63 bits of data (1 bit used for variant tag)
///     - an enum with 3 variants can have at most 62 bits of data (2 bit used for variant tag)
///     - an enum with 4 variants can still have at most 62 bits of data (2 bit used for variant tag - 4 possible values)
#[proc_macro_derive(EventToken)]
pub fn event_token(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    event_token_inner(input).into()
}
