// These lints are triggered by darling's generated code for
// `#[darling(default)]`.
#![allow(clippy::option_if_let_else)]
#![allow(clippy::needless_continue)]

use darling::{FromDeriveInput, FromMeta, util::PathList};
use heck::{ToKebabCase, ToUpperCamelCase};
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, quote};
use syn::{DeriveInput, Ident, Path, parse_macro_input, parse_quote};

#[allow(clippy::doc_markdown, reason = "false positive")]
/// Build a PascalCase enum variant name from a type path.
fn path_to_pascal_ident(path: &Path) -> Ident {
    let mut combined = String::new();
    for (index, segment) in path.segments.iter().enumerate() {
        if index > 0 {
            combined.push('_');
        }
        combined.push_str(&segment.ident.to_string());
    }
    let pascal = combined.to_upper_camel_case();
    let span = path
        .segments
        .last()
        .map_or_else(proc_macro2::Span::call_site, |segment| segment.ident.span());
    Ident::new(&pascal, span)
}

/// Parse `key = Type` meta items into a `syn::Type`.
fn parse_name_value_type(item: &syn::Meta) -> darling::Result<syn::Type> {
    let error = || darling::Error::unsupported_shape("expected `key = Type`");
    let syn::Meta::NameValue(nv) = item else {
        return Err(error());
    };
    syn::parse2(nv.value.to_token_stream()).map_err(|_| error())
}

/// Returns the kind override or the default kebab-case name from the ident.
fn default_kind(ident: &Ident, kind: Option<String>) -> String {
    kind.unwrap_or_else(|| ident.to_string().to_kebab_case())
}

/// Wrapper for `syn::Path` that parses from `key = Type` syntax.
#[derive(Debug, Clone)]
struct TypePath(Path);

impl FromMeta for TypePath {
    fn from_meta(item: &syn::Meta) -> darling::Result<Self> {
        let ty = parse_name_value_type(item)?;
        match ty {
            syn::Type::Path(type_path) if type_path.qself.is_none() => Ok(Self(type_path.path)),
            _ => Err(darling::Error::unsupported_shape("expected `key = Type`")),
        }
    }
}

/// Wrapper for `syn::Type` that parses from `key = Type` syntax.
#[derive(Debug, Clone)]
struct TypeExpr(syn::Type);

impl FromMeta for TypeExpr {
    fn from_meta(item: &syn::Meta) -> darling::Result<Self> {
        parse_name_value_type(item).map(Self)
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
    #[darling(default)]
    derives: Option<PathList>,
}

/// Configuration for the `#[projection(...)]` attribute.
#[derive(Debug, FromDeriveInput)]
#[darling(attributes(projection), supports(struct_any))]
struct ProjectionArgs {
    ident: Ident,
    #[darling(default)]
    kind: Option<String>,
    #[darling(default)]
    id: Option<TypeExpr>,
    #[darling(default)]
    instance_id: Option<TypeExpr>,
    #[darling(default)]
    metadata: Option<TypeExpr>,
    #[darling(default)]
    events: PathList,
}

/// Captures the event type path and its generated enum variant identifier.
struct EventSpec<'a> {
    path: &'a Path,
    variant: Ident,
}

impl<'a> EventSpec<'a> {
    /// Build an event spec from a type path.
    fn new(path: &'a Path) -> Self {
        Self {
            path,
            variant: path_to_pascal_ident(path),
        }
    }
}

/// Parse derive input with darling and render errors as tokens.
fn parse_or_error<T, F>(input: &DeriveInput, f: F) -> TokenStream2
where
    T: FromDeriveInput,
    F: FnOnce(T) -> TokenStream2,
{
    match T::from_derive_input(input) {
        Ok(args) => f(args),
        Err(err) => err.write_errors(),
    }
}

/// Derives the `Aggregate` trait for a struct.
///
/// This macro generates:
/// - An event enum containing all aggregate event types
/// - `EventKind` trait implementation for runtime kind dispatch
/// - `ProjectionEvent` trait implementation for event deserialisation
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
/// - `derives(Trait1, Trait2, ...)` - Additional derives for the generated
///   event enum. Always includes `Clone` and `serde::Serialize`. Common
///   additions: `Debug`, `PartialEq`, `Eq`
///
/// # Example
///
/// ```ignore
/// #[derive(Aggregate)]
/// #[aggregate(
///     id = String,
///     error = String,
///     events(FundsDeposited, FundsWithdrawn),
///     derives(Debug, PartialEq, Eq)
/// )]
/// pub struct Account {
///     balance: i64,
/// }
/// ```
#[proc_macro_derive(Aggregate, attributes(aggregate))]
pub fn derive_aggregate(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    derive_aggregate_impl(&input).into()
}

/// Internal entry point that returns tokens for the aggregate derive.
fn derive_aggregate_impl(input: &DeriveInput) -> TokenStream2 {
    parse_or_error::<AggregateArgs, _>(input, |args| generate_aggregate_impl(args, input))
}

/// Generate the aggregate derive implementation tokens.
fn generate_aggregate_impl(args: AggregateArgs, input: &DeriveInput) -> TokenStream2 {
    let event_specs: Vec<EventSpec<'_>> = args.events.iter().map(EventSpec::new).collect();

    if event_specs.is_empty() {
        return darling::Error::custom("events(...) must contain at least one event type")
            .with_span(&input.ident)
            .write_errors();
    }

    let struct_name = &args.ident;
    let struct_vis = &args.vis;
    let id_type = &args.id.0;
    let error_type = &args.error.0;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let kind = default_kind(struct_name, args.kind);

    let event_enum_name = args.event_enum.map_or_else(
        || Ident::new(&format!("{struct_name}Event"), struct_name.span()),
        |name| Ident::new(&name, struct_name.span()),
    );

    let event_types: Vec<&Path> = event_specs.iter().map(|spec| spec.path).collect();
    let variant_names: Vec<&Ident> = event_specs.iter().map(|spec| &spec.variant).collect();

    // Build derives list - always include Clone, add user-specified traits
    let derives = if let Some(user_derives) = &args.derives {
        let user_paths: Vec<&Path> = user_derives.iter().collect();
        quote! { #[derive(Clone, #(#user_paths),*)] }
    } else {
        quote! { #[derive(Clone)] }
    };

    let expanded = quote! {
        #derives
        #struct_vis enum #event_enum_name {
            #(#variant_names(#event_types)),*
        }

        impl ::sourcery::event::EventKind for #event_enum_name {
            fn kind(&self) -> &'static str {
                match self {
                    #(Self::#variant_names(_) => #event_types::KIND),*
                }
            }
        }

        impl ::serde::Serialize for #event_enum_name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: ::serde::Serializer,
            {
                match self {
                    #(Self::#variant_names(inner) => ::serde::Serialize::serialize(inner, serializer)),*
                }
            }
        }

        impl ::sourcery::ProjectionEvent for #event_enum_name {
            const EVENT_KINDS: &'static [&'static str] = &[#(#event_types::KIND),*];

            fn from_stored<S: ::sourcery::store::EventStore>(
                stored: &::sourcery::store::StoredEvent<S::Id, S::Position, S::Data, S::Metadata>,
                store: &S,
            ) -> Result<Self, ::sourcery::event::EventDecodeError<S::Error>> {
                match stored.kind() {
                    #(#event_types::KIND => Ok(Self::#variant_names(
                        store.decode_event(stored).map_err(::sourcery::event::EventDecodeError::Store)?
                    )),)*
                    _ => Err(::sourcery::event::EventDecodeError::UnknownKind {
                        kind: stored.kind().to_string(),
                        expected: Self::EVENT_KINDS,
                    }),
                }
            }
        }

        #(
            impl From<#event_types> for #event_enum_name {
                fn from(event: #event_types) -> Self {
                    Self::#variant_names(event)
                }
            }
        )*

        impl #impl_generics ::sourcery::Aggregate for #struct_name #ty_generics #where_clause {
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
/// This macro always generates:
/// - `Projection` trait implementation with `KIND` constant
///
/// It can also generate [`ProjectionFilters`] for the common case via
/// `events(...)` (or explicit `id` / `instance_id` / `metadata`
/// attributes), using `Default` for initialisation and a simple
/// `Filters::new().event::<E>()...` filter set.
///
/// # Attributes
///
/// ## Optional
/// - `kind = "name"` - Projection type identifier (default: kebab-case struct
///   name)
/// - `events(Event1, Event2, ...)` - Auto-generate `ProjectionFilters::filters`
///   with global event subscriptions.
/// - `id = Type` - Override `ProjectionFilters::Id` (default: `String` when
///   auto-generating `ProjectionFilters`)
/// - `instance_id = Type` - Override `ProjectionFilters::InstanceId` (default:
///   `()` when auto-generating `ProjectionFilters`)
/// - `metadata = Type` - Override `ProjectionFilters::Metadata` (default: `()`
///   when auto-generating `ProjectionFilters`)
///
/// # Example
///
/// ```ignore
/// #[derive(Default, Projection)]
/// pub struct AccountLedger {
///     total: i64,
/// }
/// ```
#[proc_macro_derive(Projection, attributes(projection))]
pub fn derive_projection(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    derive_projection_impl(&input).into()
}

/// Internal entry point that returns tokens for the projection derive.
fn derive_projection_impl(input: &DeriveInput) -> TokenStream2 {
    parse_or_error::<ProjectionArgs, _>(input, |args| generate_projection_impl(args, input))
}

/// Generate the projection derive implementation tokens.
fn generate_projection_impl(args: ProjectionArgs, input: &DeriveInput) -> TokenStream2 {
    let struct_name = &args.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let kind = default_kind(struct_name, args.kind);

    let projection_impl = quote! {
        impl #impl_generics ::sourcery::Projection for #struct_name #ty_generics #where_clause {
            const KIND: &'static str = #kind;
        }
    };

    let auto_projection_filters = !args.events.is_empty()
        || args.id.is_some()
        || args.instance_id.is_some()
        || args.metadata.is_some();

    if !auto_projection_filters {
        return projection_impl;
    }

    let id_ty = args.id.map_or_else(|| parse_quote!(String), |ty| ty.0);
    let instance_id_ty = args.instance_id.map_or_else(|| parse_quote!(()), |ty| ty.0);
    let metadata_ty = args.metadata.map_or_else(|| parse_quote!(()), |ty| ty.0);
    let subscribed_events: Vec<&Path> = args.events.iter().collect();

    let filters_body = if subscribed_events.is_empty() {
        quote! { ::sourcery::Filters::new() }
    } else {
        quote! { ::sourcery::Filters::new() #(.event::<#subscribed_events>())* }
    };

    let projection_filters_where = if let Some(where_clause) = where_clause {
        let predicates = &where_clause.predicates;
        quote! {
            where
                #predicates,
                #struct_name #ty_generics: ::core::default::Default
        }
    } else {
        quote! {
            where
                #struct_name #ty_generics: ::core::default::Default
        }
    };

    quote! {
        #projection_impl

        impl #impl_generics ::sourcery::ProjectionFilters for #struct_name #ty_generics
        #projection_filters_where
        {
            type Id = #id_ty;
            type InstanceId = #instance_id_ty;
            type Metadata = #metadata_ty;

            fn init(_instance_id: &Self::InstanceId) -> Self {
                Self::default()
            }

            fn filters<S>(_instance_id: &Self::InstanceId) -> ::sourcery::Filters<S, Self>
            where
                S: ::sourcery::store::EventStore<Id = Self::Id, Metadata = Self::Metadata>,
            {
                #filters_body
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use syn::parse_quote;

    use super::*;

    /// Normalise token output by removing whitespace.
    fn compact(tokens: &TokenStream2) -> String {
        tokens
            .to_string()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    #[test]
    /// Verifies path parsing for `id = Type` syntax.
    fn type_path_parses_name_value_path() {
        let meta: syn::Meta = parse_quote!(id = String);
        let parsed = TypePath::from_meta(&meta).unwrap();
        assert_eq!(parsed.0, parse_quote!(String));
    }

    #[test]
    /// Ensures non-path values are rejected for `TypePath`.
    fn type_path_rejects_non_path_value() {
        let meta: syn::Meta = parse_quote!(id = "String");
        let err = TypePath::from_meta(&meta).unwrap_err();
        assert!(err.to_string().contains("expected `key = Type`"));
    }

    #[test]
    /// Confirms default kind and event enum names are generated.
    fn generate_aggregate_impl_uses_default_kind_and_event_enum() {
        let input: DeriveInput = parse_quote! {
            #[aggregate(id = String, error = String, events(FundsDeposited))]
            pub struct Account {
                balance: i64,
            }
        };

        let expanded = derive_aggregate_impl(&input);
        let compact = compact(&expanded);

        assert!(compact.contains("enumAccountEvent"));
        assert!(compact.contains("impl::sourcery::AggregateforAccount"));
        assert!(compact.contains("constKIND:&'staticstr=\"account\""));
    }

    #[test]
    /// Confirms explicit kind and enum overrides are honored.
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

        let expanded = derive_aggregate_impl(&input);
        let compact = compact(&expanded);

        assert!(compact.contains("enumBankAccountEvent"));
        assert!(compact.contains("constKIND:&'staticstr=\"bank-account\""));
    }

    #[test]
    /// Ensures empty event lists yield a compile-time error.
    fn generate_aggregate_impl_emits_error_on_empty_events_list() {
        let input: DeriveInput = parse_quote! {
            #[aggregate(id = String, error = String, events())]
            pub struct Account;
        };

        let expanded = derive_aggregate_impl(&input);
        let compact = compact(&expanded);

        assert!(compact.contains("events(...)mustcontainatleastoneeventtype"));
    }

    #[test]
    /// Confirms default kind for projections (no attributes required).
    fn generate_projection_impl_uses_default_kind() {
        let input: DeriveInput = parse_quote! {
            pub struct AccountLedger;
        };

        let expanded = derive_projection_impl(&input);
        let compact = compact(&expanded);

        assert!(compact.contains("impl::sourcery::ProjectionforAccountLedger"));
        assert!(compact.contains("constKIND:&'staticstr=\"account-ledger\""));
    }

    #[test]
    /// Confirms projection kind override is honored.
    fn generate_projection_impl_respects_kind_override() {
        let input: DeriveInput = parse_quote! {
            #[projection(kind = "custom-ledger")]
            pub struct AccountLedger;
        };

        let expanded = derive_projection_impl(&input);
        let compact = compact(&expanded);

        assert!(compact.contains("constKIND:&'staticstr=\"custom-ledger\""));
    }

    #[test]
    /// Confirms events(...) generates a [`ProjectionFilters`] impl with
    /// defaults.
    fn generate_projection_impl_with_events_generates_projection_filters() {
        let input: DeriveInput = parse_quote! {
            #[projection(events(FundsDeposited, FundsWithdrawn))]
            pub struct AccountLedger;
        };

        let expanded = derive_projection_impl(&input);
        let compact = compact(&expanded);

        assert!(compact.contains("impl::sourcery::ProjectionFiltersforAccountLedger"));
        assert!(compact.contains("typeId=String"));
        assert!(compact.contains("typeInstanceId=()"));
        assert!(compact.contains("typeMetadata=()"));
        assert!(compact.contains("event::<FundsDeposited>()"));
        assert!(compact.contains("event::<FundsWithdrawn>()"));
    }

    #[test]
    /// Confirms projection filter type overrides are honored.
    fn generate_projection_impl_respects_projection_filter_type_overrides() {
        let input: DeriveInput = parse_quote! {
            #[projection(
                id = uuid::Uuid,
                instance_id = String,
                metadata = EventMetadata,
                events(FundsDeposited)
            )]
            pub struct AccountLedger;
        };

        let expanded = derive_projection_impl(&input);
        let compact = compact(&expanded);

        assert!(compact.contains("typeId=uuid::Uuid"));
        assert!(compact.contains("typeInstanceId=String"));
        assert!(compact.contains("typeMetadata=EventMetadata"));
    }
}
