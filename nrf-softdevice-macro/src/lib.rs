extern crate proc_macro;

use std::str::FromStr;

use darling::{Error, FromMeta};
use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote, quote_spanned, ToTokens};
use syn::parse::Parse;
use syn::spanned::Spanned;
use syn::{parenthesized, Ident, LitStr, Token};

use crate::ctxt::Ctxt;
use crate::security_mode::SecurityMode;

mod ctxt;
mod security_mode;
mod uuid;

use strum_macros::EnumString;

use crate::uuid::Uuid;

#[derive(Debug, FromMeta)]
struct ServiceArgs {
    uuid: Uuid,
}

#[derive(Debug, FromMeta)]
struct CharacteristicArgs {
    uuid: Uuid,
    #[darling(default)]
    read: bool,
    #[darling(default)]
    write: bool,
    #[darling(default)]
    write_without_response: bool,
    #[darling(default)]
    notify: bool,
    #[darling(default)]
    indicate: bool,
    #[darling(default)]
    security: Option<SecurityMode>,
}

#[derive(Debug)]
struct Characteristic {
    name: String,
    ty: syn::Type,
    args: CharacteristicArgs,
    span: Span,
    vis: syn::Visibility,
}

#[proc_macro_attribute]
pub fn gatt_server(_args: TokenStream, item: TokenStream) -> TokenStream {
    // Context for error reporting
    let ctxt = Ctxt::new();

    let mut struc = syn::parse_macro_input!(item as syn::ItemStruct);

    let struct_vis = &struc.vis;
    let struct_fields = match &mut struc.fields {
        syn::Fields::Named(n) => n,
        _ => {
            let s = struc.ident;

            ctxt.error_spanned_by(s, "gatt_server structs must have named fields, not tuples.");

            return TokenStream::new();
        }
    };
    let fields = struct_fields.named.iter().cloned().collect::<Vec<syn::Field>>();

    let struct_name = struc.ident.clone();
    let event_enum_name = format_ident!("{}Event", struct_name);

    let mut code_register_init = TokenStream2::new();
    let mut code_on_write = TokenStream2::new();
    let mut code_event_enum = TokenStream2::new();

    let ble = quote!(::nrf_softdevice::ble);

    for field in fields.iter() {
        let name = field.ident.as_ref().unwrap();
        let ty = &field.ty;
        let span = field.ty.span();
        code_register_init.extend(quote_spanned!(span=>
            #name: #ty::new(sd)?,
        ));

        if let syn::Type::Path(p) = &field.ty {
            let name_pascal = format_ident!("{}", inflector::cases::pascalcase::to_pascal_case(&name.to_string()));
            let event_enum_ty = p.path.get_ident().unwrap();
            let event_enum_variant = format_ident!("{}Event", event_enum_ty);
            code_event_enum.extend(quote_spanned!(span=>
                #name_pascal(#event_enum_variant),
            ));

            code_on_write.extend(quote_spanned!(span=>
                if let Some(e) = self.#name.on_write(handle, data) {
                    return Some(#event_enum_name::#name_pascal(e));
                }
            ));
        }
    }

    struct_fields.named = syn::punctuated::Punctuated::from_iter(fields);
    let struc_vis = struc.vis.clone();

    let result = quote! {
        #struc

        impl #struct_name {
            #struct_vis fn new(sd: &mut ::nrf_softdevice::Softdevice) -> Result<Self, #ble::gatt_server::RegisterError>
            {
                Ok(Self {
                    #code_register_init
                })
            }
        }

        #struc_vis enum #event_enum_name {
            #code_event_enum
        }

        impl #ble::gatt_server::Server for #struct_name {
            type Event = #event_enum_name;

            fn on_write(&self, _conn: &::nrf_softdevice::ble::Connection, handle: u16, op: ::nrf_softdevice::ble::gatt_server::WriteOp, offset: usize, data: &[u8]) -> Option<Self::Event> {
                use #ble::gatt_server::Service;

                #code_on_write
                None
            }
        }
    };

    match ctxt.check() {
        Ok(()) => result.into(),
        Err(e) => e.into(),
    }
}

#[proc_macro_attribute]
pub fn gatt_service(args: TokenStream, item: TokenStream) -> TokenStream {
    let args = syn::parse_macro_input!(args as syn::AttributeArgs);
    let mut struc = syn::parse_macro_input!(item as syn::ItemStruct);

    let ctxt = Ctxt::new();

    let args = match ServiceArgs::from_list(&args) {
        Ok(v) => v,
        Err(e) => {
            ctxt.error_spanned_by(e.write_errors(), "ServiceArgs Parsing failed");
            return ctxt.check().unwrap_err().into();
        }
    };

    let mut chars = Vec::new();

    let struct_vis = &struc.vis;
    let struct_fields = match &mut struc.fields {
        syn::Fields::Named(n) => n,
        _ => {
            let s = struc.ident;

            ctxt.error_spanned_by(s, "gatt_service structs must have named fields, not tuples.");

            return ctxt.check().unwrap_err().into();
        }
    };
    let mut fields = struct_fields.named.iter().cloned().collect::<Vec<syn::Field>>();
    let mut err: Option<Error> = None;
    fields.retain(|field| {
        if let Some(attr) = field
            .attrs
            .iter()
            .find(|attr| attr.path.segments.len() == 1 && attr.path.segments.first().unwrap().ident == "characteristic")
        {
            let args = attr.parse_meta().unwrap();

            let args = match CharacteristicArgs::from_meta(&args) {
                Ok(v) => v,
                Err(e) => {
                    err = Some(e);
                    return false;
                }
            };

            chars.push(Characteristic {
                name: field.ident.as_ref().unwrap().to_string(),
                ty: field.ty.clone(),
                args,
                span: field.ty.span(),
                vis: field.vis.clone(),
            });

            false
        } else {
            true
        }
    });

    if let Some(err) = err {
        let desc = err.to_string();
        ctxt.error_spanned_by(
            err.write_errors(),
            format!("Parsing characteristics was unsuccessful: {}", desc),
        );
        return ctxt.check().unwrap_err().into();
    }

    //panic!("chars {:?}", chars);
    let struct_name = struc.ident.clone();
    let event_enum_name = format_ident!("{}Event", struct_name);

    let mut code_impl = TokenStream2::new();
    let mut code_build_chars = TokenStream2::new();
    let mut code_struct_init = TokenStream2::new();
    let mut code_on_write = TokenStream2::new();
    let mut code_event_enum = TokenStream2::new();

    let ble = quote!(::nrf_softdevice::ble);

    for ch in &chars {
        let name_pascal = inflector::cases::pascalcase::to_pascal_case(&ch.name);
        let char_name = format_ident!("{}", ch.name);
        let value_handle = format_ident!("{}_value_handle", ch.name);
        let cccd_handle = format_ident!("{}_cccd_handle", ch.name);
        let get_fn = format_ident!("{}_get", ch.name);
        let set_fn = format_ident!("{}_set", ch.name);
        let notify_fn = format_ident!("{}_notify", ch.name);
        let indicate_fn = format_ident!("{}_indicate", ch.name);
        let fn_vis = ch.vis.clone();

        let uuid = ch.args.uuid;
        let read = ch.args.read;
        let write = ch.args.write;
        let write_without_response = ch.args.write_without_response;
        let notify = ch.args.notify;
        let indicate = ch.args.indicate;
        let ty = &ch.ty;
        let ty_as_val = quote!(<#ty as #ble::GattValue>);

        let security = if let Some(security) = ch.args.security {
            let security_inner = security.to_token_stream();
            quote!(attr = attr.read_security(#security_inner).write_security(#security_inner))
        } else {
            quote!()
        };

        fields.push(syn::Field {
            ident: Some(value_handle.clone()),
            ty: syn::Type::Verbatim(quote!(u16)),
            attrs: Vec::new(),
            colon_token: Default::default(),
            vis: syn::Visibility::Inherited,
        });

        code_build_chars.extend(quote_spanned!(ch.span=>
            let #char_name = {
                let val = [123u8; #ty_as_val::MIN_SIZE];
                let mut attr = #ble::gatt_server::characteristic::Attribute::new(&val);
                if #ty_as_val::MAX_SIZE != #ty_as_val::MIN_SIZE {
                    attr = attr.variable_len(#ty_as_val::MAX_SIZE as u16);
                }
                #security;
                let props = #ble::gatt_server::characteristic::Properties {
                    read: #read,
                    write: #write,
                    write_without_response: #write_without_response,
                    notify: #notify,
                    indicate: #indicate,
                    ..Default::default()
                };
                let metadata = #ble::gatt_server::characteristic::Metadata::new(props);
                service_builder.add_characteristic(#uuid, attr, metadata)?.build()
            };
        ));

        code_struct_init.extend(quote_spanned!(ch.span=>
            #value_handle: #char_name.value_handle,
        ));

        code_impl.extend(quote_spanned!(ch.span=>
            #fn_vis fn #get_fn(&self) -> Result<#ty, #ble::gatt_server::GetValueError> {
                let sd = unsafe { ::nrf_softdevice::Softdevice::steal() };
                let buf = &mut [0u8; #ty_as_val::MAX_SIZE];
                let size = #ble::gatt_server::get_value(sd, self.#value_handle, buf)?;
                Ok(#ty_as_val::from_gatt(&buf[..size]))
            }

            #fn_vis fn #set_fn(&self, val: &#ty) -> Result<(), #ble::gatt_server::SetValueError> {
                let sd = unsafe { ::nrf_softdevice::Softdevice::steal() };
                let buf = #ty_as_val::to_gatt(val);
                #ble::gatt_server::set_value(sd, self.#value_handle, buf)
            }
        ));

        if indicate || notify {
            fields.push(syn::Field {
                ident: Some(cccd_handle.clone()),
                ty: syn::Type::Verbatim(quote!(u16)),
                attrs: Vec::new(),
                colon_token: Default::default(),
                vis: syn::Visibility::Inherited,
            });
            code_struct_init.extend(quote_spanned!(ch.span=>
                #cccd_handle: #char_name.cccd_handle,
            ));
        }

        if write || write_without_response {
            let case_write = format_ident!("{}Write", name_pascal);
            code_event_enum.extend(quote_spanned!(ch.span=>
                #case_write(#ty),
            ));
            code_on_write.extend(quote_spanned!(ch.span=>
                if handle == self.#value_handle {
                    if data.len() < #ty_as_val::MIN_SIZE {
                        return self.#get_fn().ok().map(#event_enum_name::#case_write);
                    } else {
                        return Some(#event_enum_name::#case_write(#ty_as_val::from_gatt(data)));
                    }
                }
            ));
        }

        if notify {
            code_impl.extend(quote_spanned!(ch.span=>
                #fn_vis fn #notify_fn(
                    &self,
                    conn: &#ble::Connection,
                    val: &#ty,
                ) -> Result<(), #ble::gatt_server::NotifyValueError> {
                    let buf = #ty_as_val::to_gatt(val);
                    #ble::gatt_server::notify_value(conn, self.#value_handle, buf)
                }
            ));

            if !indicate {
                let case_cccd_write = format_ident!("{}CccdWrite", name_pascal);

                code_event_enum.extend(quote_spanned!(ch.span=>
                    #case_cccd_write{notifications: bool},
                ));
                code_on_write.extend(quote_spanned!(ch.span=>
                    if handle == self.#cccd_handle && !data.is_empty() {
                        match data[0] & 0x01 {
                            0x00 => return Some(#event_enum_name::#case_cccd_write{notifications: false}),
                            0x01 => return Some(#event_enum_name::#case_cccd_write{notifications: true}),
                            _ => {},
                        }
                    }
                ));
            }
        }

        if indicate {
            code_impl.extend(quote_spanned!(ch.span=>
                #fn_vis fn #indicate_fn(
                    &self,
                    conn: &#ble::Connection,
                    val: &#ty,
                ) -> Result<(), #ble::gatt_server::IndicateValueError> {
                    let buf = #ty_as_val::to_gatt(val);
                    #ble::gatt_server::indicate_value(conn, self.#value_handle, buf)
                }
            ));

            if !notify {
                let case_cccd_write = format_ident!("{}CccdWrite", name_pascal);

                code_event_enum.extend(quote_spanned!(ch.span=>
                    #case_cccd_write{indications: bool},
                ));
                code_on_write.extend(quote_spanned!(ch.span=>
                    if handle == self.#cccd_handle && !data.is_empty() {
                        match data[0] & 0x02 {
                            0x00 => return Some(#event_enum_name::#case_cccd_write{indications: false}),
                            0x02 => return Some(#event_enum_name::#case_cccd_write{indications: true}),
                            _ => {},
                        }
                    }
                ));
            }
        }

        if indicate && notify {
            let case_cccd_write = format_ident!("{}CccdWrite", name_pascal);

            code_event_enum.extend(quote_spanned!(ch.span=>
                #case_cccd_write{indications: bool, notifications: bool},
            ));
            code_on_write.extend(quote_spanned!(ch.span=>
                if handle == self.#cccd_handle && !data.is_empty() {
                    match data[0] & 0x03 {
                        0x00 => return Some(#event_enum_name::#case_cccd_write{indications: false, notifications: false}),
                        0x01 => return Some(#event_enum_name::#case_cccd_write{indications: false, notifications: true}),
                        0x02 => return Some(#event_enum_name::#case_cccd_write{indications: true, notifications: false}),
                        0x03 => return Some(#event_enum_name::#case_cccd_write{indications: true, notifications: true}),
                        _ => {},
                    }
                }
            ));
        }
    }

    let uuid = args.uuid;
    struct_fields.named = syn::punctuated::Punctuated::from_iter(fields);
    let struc_vis = struc.vis.clone();

    let result = quote! {
        #struc

        #[allow(unused)]
        impl #struct_name {
            #struct_vis fn new(sd: &mut ::nrf_softdevice::Softdevice) -> Result<Self, #ble::gatt_server::RegisterError>
            {
                let mut service_builder = #ble::gatt_server::builder::ServiceBuilder::new(sd, #uuid)?;

                #code_build_chars

                let _ = service_builder.build();

                Ok(Self {
                    #code_struct_init
                })
            }

            #code_impl
        }

        impl #ble::gatt_server::Service for #struct_name {
            type Event = #event_enum_name;

            fn on_write(&self, handle: u16, data: &[u8]) -> Option<Self::Event> {
                #code_on_write
                None
            }
        }

        #[allow(unused)]
        #struc_vis enum #event_enum_name {
            #code_event_enum
        }
    };
    match ctxt.check() {
        Ok(()) => result.into(),
        Err(e) => e.into(),
    }
}

#[proc_macro_attribute]
pub fn gatt_client(args: TokenStream, item: TokenStream) -> TokenStream {
    let args = syn::parse_macro_input!(args as syn::AttributeArgs);
    let mut struc = syn::parse_macro_input!(item as syn::ItemStruct);

    let ctxt = Ctxt::new();

    let args = match ServiceArgs::from_list(&args) {
        Ok(v) => v,
        Err(e) => {
            return e.write_errors().into();
        }
    };

    let mut chars = Vec::new();

    let struct_fields = match &mut struc.fields {
        syn::Fields::Named(n) => n,
        _ => {
            let s = struc.ident;

            ctxt.error_spanned_by(s, "gatt_client structs must have named fields, not tuples.");

            return TokenStream::new();
        }
    };
    let mut fields = struct_fields.named.iter().cloned().collect::<Vec<syn::Field>>();
    let mut err = None;
    fields.retain(|field| {
        if let Some(attr) = field
            .attrs
            .iter()
            .find(|attr| attr.path.segments.len() == 1 && attr.path.segments.first().unwrap().ident == "characteristic")
        {
            let args = attr.parse_meta().unwrap();

            let args = match CharacteristicArgs::from_meta(&args) {
                Ok(v) => v,
                Err(e) => {
                    err = Some(e.write_errors().into());
                    return false;
                }
            };

            chars.push(Characteristic {
                name: field.ident.as_ref().unwrap().to_string(),
                ty: field.ty.clone(),
                args,
                span: field.ty.span(),
                vis: field.vis.clone(),
            });

            false
        } else {
            true
        }
    });

    if let Some(err) = err {
        return err;
    }

    //panic!("chars {:?}", chars);
    let struct_name = struc.ident.clone();
    let event_enum_name = format_ident!("{}Event", struct_name);

    let mut code_impl = TokenStream2::new();
    let mut code_disc_new = TokenStream2::new();
    let mut code_disc_char = TokenStream2::new();
    let mut code_disc_done = TokenStream2::new();
    let mut code_event_enum = TokenStream2::new();

    let ble = quote!(::nrf_softdevice::ble);

    fields.push(syn::Field {
        ident: Some(format_ident!("conn")),
        ty: syn::Type::Verbatim(quote!(#ble::Connection)),
        attrs: Vec::new(),
        colon_token: Default::default(),
        vis: syn::Visibility::Inherited,
    });

    for ch in &chars {
        let name_pascal = inflector::cases::pascalcase::to_pascal_case(&ch.name);
        let uuid_field = format_ident!("{}_uuid", ch.name);
        let value_handle = format_ident!("{}_value_handle", ch.name);
        let cccd_handle = format_ident!("{}_cccd_handle", ch.name);
        let read_fn = format_ident!("{}_read", ch.name);
        let write_fn = format_ident!("{}_write", ch.name);
        let write_wor_fn = format_ident!("{}_write_without_response", ch.name);
        let write_try_wor_fn = format_ident!("{}_try_write_without_response", ch.name);
        let fn_vis = ch.vis.clone();

        let uuid = ch.args.uuid;
        let read = ch.args.read;
        let write = ch.args.write;
        let notify = ch.args.notify;
        let indicate = ch.args.indicate;
        let ty = &ch.ty;
        let ty_as_val = quote!(<#ty as #ble::GattValue>);

        fields.push(syn::Field {
            ident: Some(value_handle.clone()),
            ty: syn::Type::Verbatim(quote!(u16)),
            attrs: Vec::new(),
            colon_token: Default::default(),
            vis: syn::Visibility::Inherited,
        });

        fields.push(syn::Field {
            ident: Some(uuid_field.clone()),
            ty: syn::Type::Verbatim(quote!(#ble::Uuid)),
            attrs: Vec::new(),
            colon_token: Default::default(),
            vis: syn::Visibility::Inherited,
        });

        code_disc_new.extend(quote_spanned!(ch.span=>
            #value_handle: 0,
            #uuid_field: #uuid,
        ));

        let mut code_descs = TokenStream2::new();
        if indicate || notify {
            code_descs.extend(quote_spanned!(ch.span=>
                if _desc_uuid == #ble::Uuid::new_16(::nrf_softdevice::raw::BLE_UUID_DESCRIPTOR_CLIENT_CHAR_CONFIG as u16) {
                    self.#cccd_handle = desc.handle;
                }
            ));
        }

        code_disc_char.extend(quote_spanned!(ch.span=>
            if let Some(char_uuid) = characteristic.uuid {
                if char_uuid == self.#uuid_field {
                    // TODO maybe check the char_props have the necessary operations allowed? read/write/notify/etc
                    self.#value_handle = characteristic.handle_value;
                    for desc in descriptors {
                        if let Some(_desc_uuid) = desc.uuid {
                            #code_descs
                        }
                    }
                }
            }
        ));

        code_disc_done.extend(quote_spanned!(ch.span=>
            if self.#value_handle == 0 {
                return Err(#ble::gatt_client::DiscoverError::ServiceIncomplete);
            }
        ));

        if read {
            code_impl.extend(quote_spanned!(ch.span=>
                #fn_vis async fn #read_fn(&self) -> Result<#ty, #ble::gatt_client::ReadError> {
                    let mut buf = [0; #ty_as_val::MAX_SIZE];
                    let len = #ble::gatt_client::read(&self.conn, self.#value_handle, &mut buf).await?;
                    Ok(#ty_as_val::from_gatt(&buf[..len]))
                }
            ));
        }

        if write {
            code_impl.extend(quote_spanned!(ch.span=>
                #fn_vis async fn #write_fn(&self, val: &#ty) -> Result<(), #ble::gatt_client::WriteError> {
                    let buf = #ty_as_val::to_gatt(val);
                    #ble::gatt_client::write(&self.conn, self.#value_handle, buf).await
                }
                #fn_vis async fn #write_wor_fn(&self, val: &#ty) -> Result<(), #ble::gatt_client::WriteError> {
                    let buf = #ty_as_val::to_gatt(val);
                    #ble::gatt_client::write_without_response(&self.conn, self.#value_handle, buf).await
                }
                #fn_vis fn #write_try_wor_fn(&self, val: &#ty) -> Result<(), #ble::gatt_client::TryWriteError> {
                    let buf = #ty_as_val::to_gatt(val);
                    #ble::gatt_client::try_write_without_response(&self.conn, self.#value_handle, buf)
                }
            ));
        }

        if indicate || notify {
            fields.push(syn::Field {
                ident: Some(cccd_handle.clone()),
                ty: syn::Type::Verbatim(quote!(u16)),
                attrs: Vec::new(),
                colon_token: Default::default(),
                vis: syn::Visibility::Inherited,
            });
            code_disc_new.extend(quote_spanned!(ch.span=>
                #cccd_handle: 0,
            ));
            code_disc_done.extend(quote_spanned!(ch.span=>
                if self.#value_handle == 0 {
                    return Err(#ble::gatt_client::DiscoverError::ServiceIncomplete);
                }
            ));
        }

        if notify {
            let case_notification = format_ident!("{}Notification", name_pascal);
            code_event_enum.extend(quote_spanned!(ch.span=>
                #case_notification(#ty),
            ));
        }
    }

    let uuid = args.uuid;
    struct_fields.named = syn::punctuated::Punctuated::from_iter(fields);

    let result = quote! {
        #struc

        #[allow(unused)]
        impl #struct_name {
            #code_impl
        }

        impl #ble::gatt_client::Client for #struct_name {
            //type Event = #event_enum_name;

            fn uuid() -> #ble::Uuid {
                #uuid
            }

            fn new_undiscovered(conn: #ble::Connection) -> Self {
                Self {
                    conn,
                    #code_disc_new
                }
            }

            fn discovered_characteristic(
                &mut self,
                characteristic: &#ble::gatt_client::Characteristic,
                descriptors: &[#ble::gatt_client::Descriptor],
            ) {
                #code_disc_char
            }

            fn discovery_complete(&mut self) -> Result<(), #ble::gatt_client::DiscoverError> {
                #code_disc_done
                Ok(())
            }
        }

        enum #event_enum_name {
            #code_event_enum
        }
    };
    match ctxt.check() {
        Ok(()) => result.into(),
        Err(e) => e.into(),
    }
}

/// Advertisement Data Generation Macro
///
/// Helpful Resources:
/// BLE Advertising Data Basics: https://docs.silabs.com/bluetooth/4.0/general/adv-and-scanning/bluetooth-adv-data-basics
/// Assigned Numbers: https://btprodspecificationrefs.blob.core.windows.net/assigned-numbers/Assigned%20Number%20Types/Assigned_Numbers.pdf
/// Core Specification Supplement 9: https://www.bluetooth.com/specifications/specs/core-specification-supplement-9/
///
/// TODO: replace panics with compiler_error

/// Helper funcs

fn half_word_to_reversed_bytes(value: u16) -> TokenStream2 {
    let big = ((value >> 8) & 0xff) as u8;
    let small = (value & 0xff) as u8;

    quote! { #small, #big }
}

/// Parsers and Types

#[derive(Debug)]
struct Set<T> {
    items: Vec<T>,
}

impl<T> Parse for Set<T>
where
    T: Parse + PartialEq,
{
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut items = Vec::new();

        let content;
        parenthesized!(content in input);

        while !content.is_empty() {
            let item: T = content.parse()?;
            if items.contains(&item) {
                panic!("Identifiers must be unique.")
            }
            items.push(item);

            if content.is_empty() {
                break;
            }

            content.parse::<Token![,]>()?;
        }

        Ok(Set { items })
    }
}

#[allow(non_camel_case_types)]
#[derive(Debug, PartialEq, Clone, Copy, EnumString)]
#[repr(u8)]
enum Flag {
    LimitedDiscovery = 0b1,
    GeneralDiscovery = 0b10,
    LE_Only = 0b100,

    // i don't understand these but in case people want them
    Bit3 = 0b1000,
    Bit4 = 0b10000,
    // the rest are "reserved for future use"
}

impl Parse for Flag {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let ident: Ident = input.parse()?;

        if let Ok(flag) = Flag::from_str(ident.to_string().as_str()) {
            Ok(flag)
        } else {
            Err(input.error("Expected flag identifier."))
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy, EnumString)]
#[repr(u16)]
enum BasicService {
    GenericAccess = 0x1800,
    GenericAttribute,
    ImmediateAlert,
    LinkLoss,
    TxPower,
    CurrentTime,
    ReferenceTimeUpdate,
    NextDSTChange,
    Glucose,
    HealthThermometer,
    DeviceInformation,
    HeartRate = 0x180d,
    PhoneAlertStatus,
    Battery,
    BloodPressure,
    AlertNotification,
    HumanInterfaceDevice,
    ScanParameters,
    RunnnigSpeedAndCadence,
    AutomationIO,
    CyclingSpeedAndCadence,
    CyclingPower = 0x1818,
    LocationAndNavigation,
    EnvironmentalSensing,
    BodyComposition,
    UserData,
    WeightScale,
    BondManagement,
    ContinousGlucoseMonitoring,
    InternetProtocolSupport,
    IndoorPositioning,
    PulseOximeter,
    HTTPProxy,
    TransportDiscovery,
    ObjectTransfer,
    FitnessMachine,
    MeshProvisioning,
    MeshProxy,
    ReconnectionConfiguration,
    InsulinDelivery = 0x183a,
    BinarySensor,
    EmergencyConfiguration,
    AuthorizationControl,
    PhysicalActivityMonitor,
    ElapsedTime,
    GenericHealthSensor,
    AudioInputControl = 0x1843,
    VolumeControl,
    VolumeOffsetControl,
    CoordinatedSetIdentification,
    DeviceTime,
    MediaControl,
    GenericMediaControl, // why??
    ConstantToneExtension,
    TelephoneBearer,
    GenericTelephoneBearer,
    MicrophoneControl,
    AudioStreamControl,
    BroadcastAudioScan,
    PublishedAudioScan,
    BasicAudioCapabilities,
    BroadcastAudioAnnouncement,
    CommonAudio,
    HearingAccess,
    TelephonyAndMediaAudio,
    PublicBroadcastAnnouncement,
    ElectronicShelfLabel,
    GamingAudio,
    MeshProxySolicitation,
}

#[derive(Debug, PartialEq)]
enum Service {
    Basic16(BasicService),
    Custom(String),
}

impl Parse for Service {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let ident: Ident = input.parse()?;

        match ident.to_string().as_str() {
            "Custom" => {
                let content;
                parenthesized!(content in input);
                let uuid: LitStr = content.parse()?;
                Ok(Self::Custom(uuid.value()))
            }
            other => {
                if let Ok(service) = BasicService::from_str(other) {
                    Ok(Service::Basic16(service))
                } else {
                    Err(input.error("Expected service identifier."))
                }
            }
        }
    }
}

#[derive(Debug)]
enum Services {
    Incomplete(u8, Vec<Service>),
    Complete(u8, Vec<Service>),
}

impl Parse for Services {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let ident: Ident = input.parse()?;
        let services: Set<Service> = input.parse()?;

        match ident.to_string().as_str() {
            "Incomplete16" => Ok(Self::Incomplete(0x02, services.items)),
            "Complete16" => Ok(Self::Complete(0x03, services.items)),
            "Incomplete32" => Ok(Self::Incomplete(0x04, services.items)),
            "Complete32" => Ok(Self::Complete(0x05, services.items)),
            "Incomplete128" => Ok(Self::Incomplete(0x06, services.items)),
            "Complete128" => Ok(Self::Complete(0x07, services.items)),
            _ => Err(input.error("Expected service list.")),
        }
    }
}

#[derive(Debug)]
struct AdvertisementData {
    flags: Option<Set<Flag>>,
    services: Option<Services>,
    short_name: Option<LitStr>,
    full_name: Option<LitStr>,
}

macro_rules! ingest_unique_pattern {
    ($NAME:ident, $TYPE:ty, $INPUT:ident) => {
        if let Some(_) = $NAME {
            panic!("Mutliple {}(s) provided.", stringify!($NAME))
        } else {
            let tmp: $TYPE = $INPUT.parse()?;
            $NAME = Some(tmp);
        }
    };
}

impl Parse for AdvertisementData {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut flag = None;
        let mut service = None;
        let mut short_name = None;
        let mut full_name = None;

        while !input.is_empty() {
            let ident: Ident = input.parse()?;
            input.parse::<syn::Token![:]>()?;

            match ident.to_string().as_str() {
                "flags" => {
                    ingest_unique_pattern!(flag, Set<Flag>, input);
                }
                "services" => {
                    ingest_unique_pattern!(service, Services, input);
                }
                "short_name" => {
                    ingest_unique_pattern!(short_name, LitStr, input);
                }
                "full_name" => {
                    ingest_unique_pattern!(full_name, LitStr, input);
                }
                unknown => {
                    panic!("Unexpected adv data field: \"{}\"", unknown);
                }
            }

            if input.is_empty() {
                break;
            }

            input.parse::<syn::Token![,]>()?;
        }

        Ok(AdvertisementData {
            flags: flag,
            services: service,
            short_name,
            full_name,
        })
    }
}

/// Renderers for each AD type

fn render_flags(input: Set<Flag>) -> (u8, TokenStream2) {
    let result = input.items.iter().fold(0, |partial, flag| partial + flag.clone() as u8);

    (
        2,
        quote! {
            2u8, 1u8, #result
        },
    )
}

fn render_services(services: Services) -> (u8, TokenStream2) {
    match services {
        Services::Incomplete(ad, services) | Services::Complete(ad, services) => {
            let mut length = 1u8;

            let renders: Vec<TokenStream2> = services
                .iter()
                .map(|service| match service {
                    Service::Basic16(service) => {
                        length += 2;
                        half_word_to_reversed_bytes(service.clone() as u16)
                    }
                    Service::Custom(string) => {
                        if let Ok(uuid) = Uuid::from_string(string) {
                            match uuid {
                                Uuid::Uuid128(bytes) => {
                                    length += 16;
                                    quote! { #(#bytes),* }
                                }
                                Uuid::Uuid16(int) => {
                                    length += 2;
                                    half_word_to_reversed_bytes(int)
                                }
                            }
                        } else {
                            panic!("Could not parse string literal as UUID.");
                        }
                    }
                })
                .collect();

            (
                length,
                quote! {
                    #length, #ad, #(#renders),*
                },
            )
        }
    }
}

fn render_name(full: bool, input: LitStr) -> (u8, TokenStream2) {
    let string = input.value();
    let length = (string.len() + 1) as u8;
    let ad = if full { 9u8 } else { 8u8 };

    let as_bytes: Vec<TokenStream2> = string
        .chars()
        .map(|c| {
            let i = c as u8;
            quote! { #i }
        })
        .collect();

    (length, quote! { #length, #ad, #(#as_bytes),* })
}

/// Helpers for macro-level logic
/// and bringing all the renderers together

macro_rules! use_renderer {
    ($DATA:ident, $LENGTH:ident, $CFGs:ident, $ATTR:ident, $ENTRY:expr) => {
        if let Some($ATTR) = $DATA.$ATTR {
            let (len, tokens) = $ENTRY;
            $LENGTH += len + 1;
            $CFGs.push(tokens);
        }
    };
}

fn generate_data(input: TokenStream) -> (u8, TokenStream) {
    let input: TokenStream2 = TokenStream2::from(input);
    let data: AdvertisementData = syn::parse2(input).unwrap();

    let mut configs: Vec<TokenStream2> = Vec::new();
    let mut length = 0u8;

    use_renderer!(data, length, configs, flags, render_flags(flags));
    use_renderer!(data, length, configs, services, render_services(services));
    use_renderer!(data, length, configs, short_name, render_name(true, short_name));
    use_renderer!(data, length, configs, full_name, render_name(true, full_name));

    (
        length,
        quote! {
            &[
                #(#configs),*
            ]
        }
        .into(),
    )
}

/// Macros

#[proc_macro]
pub fn generate_adv_data(input: TokenStream) -> TokenStream {
    let (_, tokens) = generate_data(input);

    // if len > 31 {
    //     panic!("Advertisement data may not exceed 31 bytes. Try using incomplete lists, or shortened names. You can put more info in the scan data.")
    // }

    tokens
}

// #[proc_macro]
// pub fn generate_scan_data(input: TokenStream) -> TokenStream {
//     let (len, tokens) = generate_data(input);

//     // if len > 31 {
//     //     panic!("Scan data may not exceed 31 bytes. Try using incomplete lists, or shortened names.")
//     // }

//     tokens
// }
