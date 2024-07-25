// Copyright 2020 - developers of the `grammers` project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! This module gathers all the code generation submodules and coordinates
//! them, feeding them the right data.

#![deny(unsafe_code)]

mod enums;
mod grouper;
mod metadata;
mod rustifier;
mod structs;

use grammers_tl_parser::tl::{Category, Definition, Type};
use std::{
    env,
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
};

pub struct Config {
    pub gen_name_for_id: bool,
    pub deserializable_functions: bool,
    pub impl_debug: bool,
    pub impl_from_type: bool,
    pub impl_from_enum: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            gen_name_for_id: false,
            deserializable_functions: false,
            impl_debug: true,
            impl_from_type: true,
            impl_from_enum: true,
        }
    }
}

/// Don't generate types for definitions of this type,
/// since they are "core" types and treated differently.
const SPECIAL_CASED_TYPES: [&str; 1] = ["Bool"];

fn ignore_type(ty: &Type) -> bool {
    SPECIAL_CASED_TYPES.iter().any(|&x| x == ty.name)
}

fn generate_rust_code_license(file: &mut impl Write) -> io::Result<()> {
    writeln!(
        file,
        r#"// Copyright 2020 - developers of the `grammers` project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms."#
    )?;

    Ok(())
}

fn generate_rust_code_layer(file: &mut impl Write, layer: i32) -> io::Result<()> {
    writeln!(
        file,
        r#"/// The schema layer from which the definitions were generated.
pub const LAYER: i32 = {layer};"#
    )?;

    Ok(())
}

fn generate_rust_code_name_for_id(
    file: &mut impl Write,
    definitions: &[Definition],
    config: &Config,
) -> io::Result<()> {
    if config.gen_name_for_id {
        writeln!(
            file,
            r#"/// Return the name from the `.tl` definition corresponding to the provided definition identifier.
pub fn name_for_id(id: u32) -> &'static str {{
    match id {{
        0x1cb5c415 => "vector","#
        )?;
        for def in definitions {
            writeln!(file, r#"        0x{:x} => "{}","#, def.id, def.full_name())?;
        }

        writeln!(
            file,
            r#"
        _ => "(unknown)",
    }}
}}"#,
        )?;
    }

    Ok(())
}

pub fn generate_rust_code(
    definitions: &[Definition],
    layer: i32,
    config: &Config,
) -> io::Result<()> {
    let metadata = metadata::Metadata::new(definitions);

    {
        let mut file = BufWriter::new(File::create(
            Path::new(&env::var("OUT_DIR").unwrap()).join("generated_layer.rs"),
        )?);

        generate_rust_code_license(&mut file)?;
        generate_rust_code_layer(&mut file, layer)?;

        file.flush()?;
    }

    {
        let mut file = BufWriter::new(File::create(
            Path::new(&env::var("OUT_DIR").unwrap()).join("generated_name_for_id.rs"),
        )?);

        generate_rust_code_license(&mut file)?;
        generate_rust_code_name_for_id(&mut file, definitions, config)?;

        file.flush()?;
    }

    {
        let mut file = BufWriter::new(File::create(
            Path::new(&env::var("OUT_DIR").unwrap()).join("generated_category_types.rs"),
        )?);

        generate_rust_code_license(&mut file)?;
        structs::write_category_mod(&mut file, Category::Types, definitions, &metadata, config)?;

        file.flush()?;
    }
    {
        let mut file = BufWriter::new(File::create(
            Path::new(&env::var("OUT_DIR").unwrap()).join("generated_category_funcs.rs"),
        )?);

        generate_rust_code_license(&mut file)?;
        structs::write_category_mod(
            &mut file,
            Category::Functions,
            definitions,
            &metadata,
            config,
        )?;

        file.flush()?;
    }
    {
        let mut file = BufWriter::new(File::create(
            Path::new(&env::var("OUT_DIR").unwrap()).join("generated_category_enums.rs"),
        )?);

        generate_rust_code_license(&mut file)?;
        enums::write_enums_mod(&mut file, definitions, &metadata, config)?;

        file.flush()?;
    }

    Ok(())
}
