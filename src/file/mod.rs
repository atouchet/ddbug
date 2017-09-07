use std::cmp;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};

mod dwarf;
mod elf;
mod mach;
mod pdb;

use goblin;
use memmap;
use panopticon;

use {Options, Result};
use function::{Function, FunctionOffset};
use print::{DiffList, DiffState, Print, PrintState};
use range::{Range, RangeList};
use types::{Type, TypeOffset};
use unit::Unit;
use variable::{Variable, VariableOffset};

#[derive(Debug)]
pub(crate) struct CodeRegion {
    pub machine: panopticon::Machine,
    pub region: panopticon::Region,
}

#[derive(Debug)]
pub struct File<'a, 'input> {
    path: &'a str,
    code: Option<CodeRegion>,
    sections: Vec<Section<'input>>,
    symbols: Vec<Symbol<'input>>,
    units: Vec<Unit<'input>>,
}

impl<'a, 'input> File<'a, 'input> {
    pub fn parse(path: &'a str, cb: &mut FnMut(&mut File) -> Result<()>) -> Result<()> {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(e) => {
                return Err(format!("open failed: {}", e).into());
            }
        };

        let file = match memmap::Mmap::open(&file, memmap::Protection::Read) {
            Ok(file) => file,
            Err(e) => {
                return Err(format!("memmap failed: {}", e).into());
            }
        };

        let input = unsafe { file.as_slice() };
        if input.starts_with(b"Microsoft C/C++ MSF 7.00\r\n\x1a\x44\x53\x00") {
            pdb::parse(input, path, cb)
        } else {
            let mut cursor = io::Cursor::new(input);
            match goblin::peek(&mut cursor) {
                Ok(goblin::Hint::Elf(_)) => elf::parse(input, path, cb),
                Ok(goblin::Hint::Mach(_)) => mach::parse(input, path, cb),
                Ok(_) => Err("unrecognized file format".into()),
                Err(e) => Err(format!("file identification failed: {}", e).into()),
            }
        }
    }

    fn normalize(&mut self) {
        self.symbols.sort_by(|a, b| a.address.cmp(&b.address));
        let mut used_symbols = vec![false; self.symbols.len()];

        // Set symbol names on functions/variables.
        for unit in &mut self.units {
            for function in unit.functions.values_mut() {
                if let Some(address) = function.low_pc {
                    if let Some(symbol) = Self::get_symbol(
                        &*self.symbols,
                        &mut used_symbols,
                        address,
                        function.linkage_name.or(function.name),
                    ) {
                        function.symbol_name = symbol.name;
                    }
                }
            }

            for variable in unit.variables.values_mut() {
                if let Some(address) = variable.address {
                    if let Some(symbol) = Self::get_symbol(
                        &*self.symbols,
                        &mut used_symbols,
                        address,
                        variable.linkage_name.or(variable.name),
                    ) {
                        variable.symbol_name = symbol.name;
                    }
                }
            }
        }

        // Create a unit for symbols that don't have debuginfo.
        let mut unit = Unit::default();
        unit.name = Some(b"<symtab>");
        for (index, (symbol, used)) in self.symbols.iter().zip(used_symbols.iter()).enumerate() {
            if *used {
                continue;
            }
            unit.ranges.push(Range {
                begin: symbol.address,
                end: symbol.address + symbol.size,
            });
            match symbol.ty {
                SymbolType::Variable => {
                    unit.variables.insert(
                        VariableOffset(index),
                        Variable {
                            name: symbol.name,
                            linkage_name: symbol.name,
                            address: Some(symbol.address),
                            size: Some(symbol.size),
                            ..Default::default()
                        },
                    );
                }
                SymbolType::Function => {
                    unit.functions.insert(
                        FunctionOffset(index),
                        Function {
                            name: symbol.name,
                            linkage_name: symbol.name,
                            low_pc: Some(symbol.address),
                            high_pc: Some(symbol.address + symbol.size),
                            size: Some(symbol.size),
                            ..Default::default()
                        },
                    );
                }
            }
        }
        unit.ranges.sort();
        self.units.push(unit);
    }

    // Determine if the symbol at the given address has the given name.
    // There may be multiple symbols for the same address.
    // If none match the given name, then return the first one.
    fn get_symbol<'sym>(
        symbols: &'sym [Symbol<'input>],
        used_symbols: &mut [bool],
        address: u64,
        name: Option<&'input [u8]>,
    ) -> Option<&'sym Symbol<'input>> {
        if let Ok(mut index) = symbols.binary_search_by(|x| x.address.cmp(&address)) {
            while index > 0 && symbols[index - 1].address == address {
                index -= 1;
            }
            let mut found = false;
            for symbol in &symbols[index..] {
                if symbol.address != address {
                    break;
                }
                used_symbols[index] = true;
                if symbol.name == name {
                    found = true;
                }
            }
            if found {
                None
            } else {
                Some(&symbols[index])
            }
        } else {
            None
        }
    }

    pub(crate) fn code(&self) -> Option<&CodeRegion> {
        self.code.as_ref()
    }

    fn ranges(&self) -> RangeList {
        let mut ranges = RangeList::default();
        for section in &self.sections {
            // TODO: ignore BSS for size?
            if let Some(range) = section.address() {
                ranges.push(range);
            }
        }
        for unit in &self.units {
            for range in unit.ranges.list() {
                ranges.push(*range);
            }
        }
        ranges.sort();
        ranges
    }

    pub fn print(&self, w: &mut Write, options: &Options) -> Result<()> {
        let hash = FileHash::new(self);
        let mut state = PrintState::new(self, &hash, options);

        if options.category_file {
            state.line(w, |w, _state| {
                write!(w, "file {}", self.path)?;
                Ok(())
            })?;
            state.indent(|state| {
                // TODO: display ranges/size that aren't covered by debuginfo.
                let ranges = self.ranges();
                state.list("addresses", w, &(), ranges.list())?;
                state.line_option_u64(w, "size", ranges.size())?;
                state.list("sections", w, &(), &*self.sections)?;
                // TODO: add option to display
                //state.list("symbols", w, &(), &*self.symbols)?;
                Ok(())
            })?;
            writeln!(w, "")?;
        }

        state.sort_list(w, &(), &mut *self.filter_units(state.options))
    }

    pub fn diff(w: &mut Write, file_a: &File, file_b: &File, options: &Options) -> Result<()> {
        let hash_a = FileHash::new(file_a);
        let hash_b = FileHash::new(file_b);
        let mut state = DiffState::new(file_a, &hash_a, file_b, &hash_b, options);

        if options.category_file {
            state.line(w, file_a, file_b, |w, _state, x| {
                write!(w, "file {}", x.path)?;
                Ok(())
            })?;
            state.indent(|state| {
                let ranges_a = file_a.ranges();
                let ranges_b = file_b.ranges();
                state.list("addresses", w, &(), ranges_a.list(), &(), ranges_b.list())?;
                state.line_option_u64(w, "size", ranges_a.size(), ranges_b.size())?;
                // TODO: sort sections
                state.list("sections", w, &(), &*file_a.sections, &(), &*file_b.sections)?;
                // TODO: sort symbols
                // TODO: add option to display
                //state.list("symbols", w, &(), &*file_a.symbols, &(), &*file_b.symbols)?;
                Ok(())
            })?;
            writeln!(w, "")?;
        }

        state.sort_list(
            w,
            &(),
            &mut *file_a.filter_units(state.options),
            &(),
            &mut *file_b.filter_units(state.options),
        )
    }

    fn filter_units(&self, options: &Options) -> Vec<&Unit> {
        self.units.iter().filter(|a| a.filter(options)).collect()
    }
}

#[derive(Debug)]
pub(crate) struct FileHash<'a, 'input>
where
    'input: 'a,
{
    // All functions by address.
    pub functions: HashMap<u64, &'a Function<'input>>,
    // All types by offset.
    pub types: HashMap<TypeOffset, &'a Type<'input>>,
}

impl<'a, 'input> FileHash<'a, 'input> {
    fn new(file: &'a File<'a, 'input>) -> Self {
        FileHash {
            functions: Self::functions(file),
            types: Self::types(file),
        }
    }

    /// Returns a map from address to function for all functions in the file.
    fn functions(file: &'a File<'a, 'input>) -> HashMap<u64, &'a Function<'input>> {
        let mut functions = HashMap::new();
        // TODO: insert symbol table names too
        for unit in &file.units {
            for function in unit.functions.values() {
                if let Some(low_pc) = function.low_pc {
                    // TODO: handle duplicate addresses
                    functions.insert(low_pc, function);
                }
            }
        }
        functions
    }

    /// Returns a map from offset to type for all types in the file.
    fn types(file: &'a File<'a, 'input>) -> HashMap<TypeOffset, &'a Type<'input>> {
        let mut types = HashMap::new();
        for unit in &file.units {
            for (offset, ty) in unit.types.iter() {
                types.insert(*offset, ty);
            }
        }
        types
    }
}

#[derive(Debug)]
pub(crate) struct Section<'input> {
    name: Option<&'input [u8]>,
    address: Option<u64>,
    size: u64,
}

impl<'input> Section<'input> {
    fn address(&self) -> Option<Range> {
        self.address.map(|address| {
            Range {
                begin: address,
                end: address + self.size,
            }
        })
    }

    fn print_name(&self, w: &mut Write) -> Result<()> {
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<anon-section>")?,
        }
        Ok(())
    }

    fn print_address(&self, w: &mut Write) -> Result<()> {
        if let Some(address) = self.address() {
            write!(w, "address: ")?;
            address.print(w)?;
        }
        Ok(())
    }
}

impl<'input> Print for Section<'input> {
    type Arg = ();

    fn print(&self, w: &mut Write, state: &mut PrintState, _arg: &()) -> Result<()> {
        state.line(w, |w, _state| self.print_name(w))?;
        state.indent(|state| {
            state.line_option(w, |w, _state| self.print_address(w))?;
            state.line_option_u64(w, "size", Some(self.size))
        })
    }

    fn diff(
        w: &mut Write,
        state: &mut DiffState,
        _arg_a: &(),
        a: &Self,
        _arg_b: &(),
        b: &Self,
    ) -> Result<()> {
        state.line(w, a, b, |w, _state, x| x.print_name(w))?;
        state.indent(|state| {
            state.line_option(w, a, b, |w, _state, x| x.print_address(w))?;
            state.line_option_u64(w, "size", Some(a.size), Some(b.size))
        })
    }
}

impl<'input> DiffList for Section<'input> {
    fn step_cost() -> usize {
        1
    }

    fn diff_cost(_state: &DiffState, _arg_a: &(), a: &Self, _arg_b: &(), b: &Self) -> usize {
        let mut cost = 0;
        if a.name.cmp(&b.name) != cmp::Ordering::Equal {
            cost += 2;
        }
        cost
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SymbolType {
    Variable,
    Function,
}

#[derive(Debug, Clone)]
pub(crate) struct Symbol<'input> {
    name: Option<&'input [u8]>,
    ty: SymbolType,
    address: u64,
    size: u64,
}

impl<'input> Symbol<'input> {
    fn address(&self) -> Range {
        Range {
            begin: self.address,
            end: self.address + self.size,
        }
    }

    fn print_name(&self, w: &mut Write) -> Result<()> {
        match self.ty {
            SymbolType::Variable => write!(w, "var ")?,
            SymbolType::Function => write!(w, "fn ")?,
        }
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn print_address(&self, w: &mut Write) -> Result<()> {
        write!(w, "address: ")?;
        self.address().print(w)?;
        Ok(())
    }
}
impl<'input> Print for Symbol<'input> {
    type Arg = ();

    fn print(&self, w: &mut Write, state: &mut PrintState, _arg: &()) -> Result<()> {
        state.line(w, |w, _state| self.print_name(w))?;
        state.indent(|state| {
            state.line_option(w, |w, _state| self.print_address(w))?;
            state.line_option_u64(w, "size", Some(self.size))
        })
    }

    fn diff(
        w: &mut Write,
        state: &mut DiffState,
        _arg_a: &(),
        a: &Self,
        _arg_b: &(),
        b: &Self,
    ) -> Result<()> {
        state.line(w, a, b, |w, _state, x| x.print_name(w))?;
        state.indent(|state| {
            state.line_option(w, a, b, |w, _state, x| x.print_address(w))?;
            state.line_option_u64(w, "size", Some(a.size), Some(b.size))
        })
    }
}

impl<'input> DiffList for Symbol<'input> {
    fn step_cost() -> usize {
        1
    }

    fn diff_cost(_state: &DiffState, _arg_a: &(), a: &Self, _arg_b: &(), b: &Self) -> usize {
        let mut cost = 0;
        if a.name.cmp(&b.name) != cmp::Ordering::Equal {
            cost += 2;
        }
        cost
    }
}