// These lints are triggered by darling's generated code for
// `#[darling(default)]`.
#![allow(clippy::option_if_let_else)]
#![allow(clippy::needless_continue)]

use darling::{FromDeriveInput, FromMeta, util::PathList};
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, quote};
use syn::{DeriveInput, Ident, Path, parse_macro_input};

/// Converts a `PascalCase` or `camelCase` string to `kebab-case`.
fn to_kebab_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('-');
            }
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    result
}

/// Wrapper for `syn::Path` that parses from `key = Type` syntax.
#[derive(Debug)]
struct TypePath(Path);

impl FromMeta for TypePath {
    fn from_meta(item: &syn::Meta) -> darling::Result<Self> {
        if let syn::Meta::NameValue(nv) = item
            && let syn::Expr::Path(expr_path) = &nv.value
        {
            return Ok(Self(expr_path.path.clone()));
        }
        Err(darling::Error::unsupported_shape("expected `key = Type`"))
    }
}

/// Wrapper for `syn::Type` that parses from `key = Type` syntax.
#[derive(Debug)]
struct TypeValue(syn::Type);

impl FromMeta for TypeValue {
    fn from_meta(item: &syn::Meta) -> darling::Result<Self> {
        if let syn::Meta::NameValue(nv) = item {
            let ty: syn::Type = syn::parse2(nv.value.to_token_stream())
                .map_err(|_| darling::Error::unsupported_shape("expected `key = Type`"))?;
            return Ok(Self(ty));
        }
        Err(darling::Error::unsupported_shape("expected `key = Type`"))
    }
}

/// Configuration for the `#[aggregate(...)]` attribute.
#[derive(Debug, FromDeriveInput)]
#[darling(attributes(aggregate), supports(struct_any))]
struct AggregateArgs {
    ident: Ident,
    vis: syn::Visibility,
    id: TypePath,
    error: TypePath,
    events: PathList,
    #[darling(default)]
    kind: Option<String>,
    #[darling(default)]
    event_enum: Option<String>,
}

/// Configuration for the `#[projection(...)]` attribute.
#[derive(Debug, FromDeriveInput)]
#[darling(attributes(projection), supports(struct_any))]
struct ProjectionArgs {
    ident: Ident,
    id: TypeValue,
    #[darling(default)]
    metadata: Option<TypeValue>,
    #[darling(default)]
    instance_id: Option<TypeValue>,
    #[darling(default)]
    kind: Option<String>,
}

/// Derives the `Aggregate` trait for a struct.
///
/// This macro generates:
/// - An event enum containing all aggregate event types
/// - `ProjectionEvent` trait implementation for event deserialization
/// - `SerializableEvent` trait implementation for event serialization
/// - `From<E>` implementations for each event type
/// - `Aggregate` trait implementation that dispatches to `Apply<E>` for events
///
/// **Note:** Commands are handled via individual `Handle<C>` trait
/// implementations. No command enum is generated - use
/// `execute_command::<Aggregate, Command>()` directly.
///
/// # Attributes
///
/// ## Required
/// - `id = Type` - Aggregate ID type
/// - `error = Type` - Error type for command handling
/// - `events(Type1, Type2, ...)` - Event types
///
/// ## Optional
/// - `kind = "name"` - Aggregate type identifier (default: lowercase struct
///   name)
/// - `event_enum = "Name"` - Override generated event enum name (default:
///   `{Struct}Event`)
///
/// # Example
///
/// ```ignore
/// #[derive(Aggregate)]
/// #[aggregate(id = String, error = String, events(FundsDeposited, FundsWithdrawn))]
/// pub struct Account {
///     balance: i64,
/// }
/// ```
#[proc_macro_derive(Aggregate, attributes(aggregate))]
pub fn derive_aggregate(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    derive_aggregate_impl(&input).into()
}

fn derive_aggregate_impl(input: &DeriveInput) -> TokenStream2 {
    match AggregateArgs::from_derive_input(input) {
        Ok(args) => generate_aggregate_impl(args, input),
        Err(err) => err.write_errors(),
    }
}

fn generate_aggregate_impl(args: AggregateArgs, input: &DeriveInput) -> TokenStream2 {
    let event_types: Vec<&Path> = args.events.iter().collect();

    if event_types.is_empty() {
        return darling::Error::custom("events(...) must contain at least one event type")
            .with_span(&input.ident)
            .write_errors();
    }

    let struct_name = &args.ident;
    let struct_vis = &args.vis;
    let id_type = &args.id.0;
    let error_type = &args.error.0;

    let kind = args
        .kind
        .unwrap_or_else(|| to_kebab_case(&struct_name.to_string()));

    let event_enum_name = args.event_enum.map_or_else(
        || Ident::new(&format!("{struct_name}Event"), struct_name.span()),
        |name| Ident::new(&name, struct_name.span()),
    );

    let variant_names: Vec<_> = event_types
        .iter()
        .map(|p| {
            &p.segments
                .last()
                .expect("event type path must have at least one segment")
                .ident
        })
        .collect();

    let expanded = quote! {
        #[derive(Clone, ::serde::Serialize, ::serde::Deserialize)]
        #struct_vis enum #event_enum_name {
            #(#variant_names(#event_types)),*
        }

        impl ::sourcery::codec::ProjectionEvent for #event_enum_name {
            const EVENT_KINDS: &'static [&'static str] = &[#(#event_types::KIND),*];

            fn from_stored<C: ::sourcery::codec::Codec>(
                kind: &str,
                data: &[u8],
                codec: &C,
            ) -> Result<Self, ::sourcery::codec::EventDecodeError<C::Error>> {
                match kind {
                    #(#event_types::KIND => Ok(Self::#variant_names(
                        codec.deserialize(data).map_err(::sourcery::codec::EventDecodeError::Codec)?
                    )),)*
                    _ => Err(::sourcery::codec::EventDecodeError::UnknownKind {
                        kind: kind.to_string(),
                        expected: Self::EVENT_KINDS,
                    }),
                }
            }
        }

        impl ::sourcery::codec::SerializableEvent for #event_enum_name {
            fn to_persistable<C: ::sourcery::codec::Codec, M>(
                self,
                codec: &C,
                metadata: M,
            ) -> Result<::sourcery::store::PersistableEvent<M>, C::Error> {
                let (kind, data) = match self {
                    #(Self::#variant_names(event) => (#event_types::KIND.to_string(), codec.serialize(&event)?)),*
                };
                Ok(::sourcery::store::PersistableEvent { kind, data, metadata })
            }
        }

        #(
            impl From<#event_types> for #event_enum_name {
                fn from(event: #event_types) -> Self {
                    Self::#variant_names(event)
                }
            }
        )*

        impl ::sourcery::Aggregate for #struct_name {
            const KIND: &'static str = #kind;
            type Event = #event_enum_name;
            type Error = #error_type;
            type Id = #id_type;

            fn apply(&mut self, event: &Self::Event) {
                match event {
                    #(#event_enum_name::#variant_names(e) => ::sourcery::Apply::apply(self, e)),*
                }
            }
        }
    };

    expanded
}

/// Derives the `Projection` trait for a struct.
///
/// This macro generates:
/// - `Projection` trait implementation
///
/// # Attributes
///
/// ## Required
/// - `id = Type` - Aggregate ID type
///
/// ## Optional
/// - `metadata = Type` - Metadata type (default: `()`)
/// - `instance_id = Type` - Projection instance identifier type (default: `()`)
/// - `kind = "name"` - Projection type identifier (default: lowercase struct
///   name)
///
/// # Example
///
/// ```ignore
/// #[derive(Projection)]
/// #[projection(id = String)]
/// pub struct AccountLedger;
/// ```
#[proc_macro_derive(Projection, attributes(projection))]
pub fn derive_projection(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    derive_projection_impl(&input).into()
}

fn derive_projection_impl(input: &DeriveInput) -> TokenStream2 {
    match ProjectionArgs::from_derive_input(input) {
        Ok(args) => generate_projection_impl(args),
        Err(err) => err.write_errors(),
    }
}

fn generate_projection_impl(args: ProjectionArgs) -> TokenStream2 {
    let struct_name = &args.ident;
    let id_type = &args.id.0;
    let default_metadata: syn::Type = syn::parse_quote!(());
    let default_instance_id: syn::Type = syn::parse_quote!(());
    let metadata_type = args
        .metadata
        .as_ref()
        .map(|value| &value.0)
        .unwrap_or(&default_metadata);
    let instance_id_type = args
        .instance_id
        .as_ref()
        .map(|value| &value.0)
        .unwrap_or(&default_instance_id);
    let kind = args
        .kind
        .unwrap_or_else(|| to_kebab_case(&struct_name.to_string()));

    quote! {
        impl ::sourcery::Projection for #struct_name {
            const KIND: &'static str = #kind;
            type Id = #id_type;
            type Metadata = #metadata_type;
            type InstanceId = #instance_id_type;
        }
    }
}

#[cfg(test)]
mod tests {
    use syn::parse_quote;

    use super::*;

    #[test]
    fn to_kebab_case_converts_pascal_and_camel() {
        assert_eq!(to_kebab_case("BankAccount"), "bank-account");
        assert_eq!(to_kebab_case("camelCase"), "camel-case");
    }

    #[test]
    fn type_path_parses_name_value_path() {
        let meta: syn::Meta = parse_quote!(id = String);
        let parsed = TypePath::from_meta(&meta).unwrap();
        assert_eq!(parsed.0, parse_quote!(String));
    }

    #[test]
    fn type_path_rejects_non_path_value() {
        let meta: syn::Meta = parse_quote!(id = "String");
        let err = TypePath::from_meta(&meta).unwrap_err();
        assert!(err.to_string().contains("expected `key = Type`"));
    }

    #[test]
    fn generate_aggregate_impl_uses_default_kind_and_event_enum() {
        let input: DeriveInput = parse_quote! {
            #[aggregate(id = String, error = String, events(FundsDeposited))]
            pub struct Account {
                balance: i64,
            }
        };

        let expanded = derive_aggregate_impl(&input).to_string();
        let compact: String = expanded.chars().filter(|c| !c.is_whitespace()).collect();

        assert!(compact.contains("enumAccountEvent"));
        assert!(compact.contains("impl::sourcery::AggregateforAccount"));
        assert!(compact.contains("constKIND:&'staticstr=\"account\""));
    }

    #[test]
    fn generate_aggregate_impl_respects_kind_and_event_enum_overrides() {
        let input: DeriveInput = parse_quote! {
            #[aggregate(
                id = String,
                error = String,
                events(FundsDeposited),
                kind = "bank-account",
                event_enum = "BankAccountEvent"
            )]
            pub struct Account {
                balance: i64,
            }
        };

        let expanded = derive_aggregate_impl(&input).to_string();
        let compact: String = expanded.chars().filter(|c| !c.is_whitespace()).collect();

        assert!(compact.contains("enumBankAccountEvent"));
        assert!(compact.contains("constKIND:&'staticstr=\"bank-account\""));
    }

    #[test]
    fn generate_aggregate_impl_emits_error_on_empty_events_list() {
        let input: DeriveInput = parse_quote! {
            #[aggregate(id = String, error = String, events())]
            pub struct Account;
        };

        let expanded = derive_aggregate_impl(&input).to_string();
        let compact: String = expanded.chars().filter(|c| !c.is_whitespace()).collect();

        assert!(compact.contains("events(...)mustcontainatleastoneeventtype"));
    }

    #[test]
    fn type_value_parses_name_value_type() {
        let meta: syn::Meta = parse_quote!(id = ());
        let parsed = TypeValue::from_meta(&meta).unwrap();
        assert_eq!(parsed.0, parse_quote!(()));
    }

    #[test]
    fn generate_projection_impl_uses_default_kind_and_types() {
        let input: DeriveInput = parse_quote! {
            #[projection(id = String)]
            pub struct AccountLedger;
        };

        let expanded = derive_projection_impl(&input).to_string();
        let compact: String = expanded.chars().filter(|c| !c.is_whitespace()).collect();

        assert!(compact.contains("impl::sourcery::ProjectionforAccountLedger"));
        assert!(compact.contains("constKIND:&'staticstr=\"account-ledger\""));
        assert!(compact.contains("typeMetadata=()"));
        assert!(compact.contains("typeInstanceId=()"));
    }

    #[test]
    fn generate_projection_impl_respects_overrides() {
        let input: DeriveInput = parse_quote! {
            #[projection(
                id = String,
                metadata = RequestMetadata,
                instance_id = ProjectionKey,
                kind = "account-ledger"
            )]
            pub struct AccountLedger;
        };

        let expanded = derive_projection_impl(&input).to_string();
        let compact: String = expanded.chars().filter(|c| !c.is_whitespace()).collect();

        assert!(compact.contains("constKIND:&'staticstr=\"account-ledger\""));
        assert!(compact.contains("typeMetadata=RequestMetadata"));
        assert!(compact.contains("typeInstanceId=ProjectionKey"));
    }
}
