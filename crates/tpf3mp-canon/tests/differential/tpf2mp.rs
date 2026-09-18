//! TPF2MP's original Lua economy, running under Lua 5.1 with the harness.
//!
//! The fixtures are compiled into the test binary, so the tests do not depend
//! on the working directory. Lua 5.1 matters: TPF2MP's rules assume that all
//! numbers are doubles, which Lua 5.3 and later no longer do.

use mlua::{Function, IntoLuaMulti, Lua, Table, Value};
use proptest::test_runner::{Config, TestRunner};
use tpf3mp_canon::lua::MAX_EXACT_INTEGER;

const HARNESS: &str = include_str!("harness.lua");

/// TPF2MP's `tests/run_economy_parity_vectors.lua`, unmodified.
pub const PARITY_GENERATOR: &str = include_str!("../tpf2mp_lua/run_economy_parity_vectors.lua");

macro_rules! modules {
    ($($name:literal),* $(,)?) => {
        [$(($name, include_str!(concat!("../tpf2mp_lua/tpf2_mp/", $name, ".lua")))),*]
    };
}

/// Every module `economy.lua` requires, as `require "tpf2_mp/<name>"` finds it.
const MODULES: [(&str, &str); 17] = modules!(
    "util",
    "hash",
    "json",
    "economy",
    "economy_costs",
    "economy_flow",
    "economy_revenue",
    "economy_difficulty",
    "economy_town_demand",
    "economy_feeder_access",
    "economy_allocation",
    "delivery_snapshot",
    "multihop_network",
    "multihop_passenger",
    "multihop_cargo",
    "transport_network_graph",
    "freight_path_pin",
);

/// Locals each module hands to the harness: functions to hook, and values to
/// export for inspection.
const LOCALS: [(&str, &[&str], &[&str]); 3] = [
    (
        "economy_flow",
        &["logitWeight", "scaledRate", "glide", "signedAdd"],
        &["EXP_TABLE", "SHARE_SCALE", "ACCUMULATOR_LIMIT"],
    ),
    ("economy_town_demand", &["upsertTown", "carriedByTown"], &[]),
    (
        "economy",
        &["saturatingAdd", "saturatingMultiply", "signedAdd"],
        &[],
    ),
];

/// The module source with the harness block inserted before its final
/// `return M`. The block only reassigns the named locals to their hooked
/// versions, after every definition, so the module behaves as before.
fn instrument(module: &str, source: &str) -> String {
    let Some((_, functions, values)) = LOCALS.iter().find(|(name, _, _)| *name == module) else {
        return source.to_owned();
    };
    let body = source
        .strip_suffix("return M\n")
        .unwrap_or_else(|| panic!("{module}.lua no longer ends with `return M`"));
    let mut block =
        String::from("-- Inserted by the TPF3-MP differential harness; not part of TPF2MP.\n");
    for name in *functions {
        block.push_str(&format!(
            "{name} = TPF3MP_HARNESS.hookLocal(\"{module}\", \"{name}\", {name})\n"
        ));
    }
    for name in *values {
        block.push_str(&format!(
            "TPF3MP_HARNESS.exports[\"{module}.{name}\"] = {name}\n"
        ));
    }
    format!("{body}{block}return M\n")
}

/// A Lua 5.1 state with TPF2MP's modules loaded and hooked.
pub struct Tpf2mp {
    lua: Lua,
    harness: Table,
}

impl Tpf2mp {
    pub fn new() -> Self {
        let lua = Lua::new();
        let version: String = lua.load("return _VERSION").eval().unwrap();
        assert_eq!(version, "Lua 5.1", "TPF2MP's rules need Lua 5.1 doubles");
        lua.load(HARNESS).set_name("@harness.lua").exec().unwrap();
        let harness: Table = lua.globals().get("TPF3MP_HARNESS").unwrap();
        let preload: Table = lua
            .globals()
            .get::<Table>("package")
            .unwrap()
            .get("preload")
            .unwrap();
        for (name, source) in MODULES {
            let chunk = lua
                .load(instrument(name, source))
                .set_name(format!("@tpf2_mp/{name}.lua"))
                .into_function()
                .unwrap();
            preload.set(format!("tpf2_mp/{name}"), chunk).unwrap();
        }
        harness
            .get::<Function>("install")
            .unwrap()
            .call::<()>(())
            .unwrap();
        Self { lua, harness }
    }

    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    pub fn harness(&self) -> &Table {
        &self.harness
    }

    /// The unhooked original of a ported function, such as
    /// `"economy_flow.logitWeight"`.
    pub fn function(&self, label: &str) -> Function {
        self.harness
            .get::<Table>("originals")
            .unwrap()
            .get::<Option<Function>>(label)
            .unwrap()
            .unwrap_or_else(|| panic!("no Lua function {label}"))
    }

    /// Call the original of a ported function and record the call as the
    /// parity trace would.
    pub fn run(&self, label: &str, args: impl IntoLuaMulti) -> Call {
        let args = args.into_lua_multi(&self.lua).unwrap();
        let mut all = mlua::MultiValue::new();
        all.push_back(Value::String(self.lua.create_string(label).unwrap()));
        all.extend(args);
        let record: Table = self
            .harness
            .get::<Function>("run")
            .unwrap()
            .call(all)
            .unwrap_or_else(|error| panic!("{label} failed: {error}"));
        Call::from_record(&record)
    }

    /// Run TPF2MP's parity-vector generator with tracing on. Returns the
    /// vectors it would have written and the table of recorded calls; read
    /// the records one at a time with [`Call::from_record`], since mlua can
    /// hold only a few thousand Lua references at once.
    pub fn run_parity_vectors(&self) -> (Table, Table) {
        let generator = self
            .lua
            .load(PARITY_GENERATOR)
            .set_name("@run_economy_parity_vectors.lua")
            .into_function()
            .unwrap();
        let vectors: Table = self
            .harness
            .get::<Function>("runParityVectors")
            .unwrap()
            .call(generator)
            .unwrap();
        (vectors, self.harness.get("trace").unwrap())
    }

    /// A value a module exported to the harness, such as
    /// `"economy_flow.EXP_TABLE"`.
    pub fn export(&self, name: &str) -> Value {
        self.harness
            .get::<Table>("exports")
            .unwrap()
            .get(name)
            .unwrap()
    }

    /// A field of a loaded module, such as `("economy_revenue", "ACCUMULATOR_LIMIT")`.
    pub fn module_field(&self, module: &str, field: &str) -> Value {
        self.lua
            .globals()
            .get::<Table>("package")
            .unwrap()
            .get::<Table>("loaded")
            .unwrap()
            .get::<Table>(format!("tpf2_mp/{module}"))
            .unwrap()
            .get(field)
            .unwrap()
    }

    pub fn table(&self) -> Table {
        self.lua.create_table().unwrap()
    }

    /// A Lua table with the given fields; `None` values stay nil.
    pub fn record(&self, fields: &[(&str, Option<i64>)]) -> Table {
        let table = self.table();
        for (name, value) in fields {
            if let Some(value) = value {
                table.set(*name, num(*value)).unwrap();
            }
        }
        table
    }
}

/// One recorded call of a ported Lua function: copies of its arguments before
/// the call, its results, and for functions that mutate their arguments,
/// copies of the arguments after it. Indices are 1-based, as in Lua.
pub struct Call {
    pub label: String,
    args: Table,
    results: Table,
    after: Option<Table>,
}

impl Call {
    pub fn from_record(record: &Table) -> Self {
        Self {
            label: string(&get(record, "label"), "label"),
            args: record.get("args").unwrap(),
            results: record.get("results").unwrap(),
            after: record.get("after").unwrap(),
        }
    }

    pub fn arg(&self, index: usize) -> Value {
        self.args.get(index).unwrap()
    }

    pub fn result(&self, index: usize) -> Value {
        self.results.get(index).unwrap()
    }

    /// An argument as it was after the call.
    pub fn after(&self, index: usize) -> Value {
        self.after
            .as_ref()
            .unwrap_or_else(|| panic!("{} records no arguments after the call", self.label))
            .get(index)
            .unwrap()
    }
}

/// How a check of one call ended. A mismatch panics instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The port returned exactly what Lua returned.
    Matched,
    /// The port returned `None`: Lua's arithmetic left the exact range or
    /// divided by zero.
    Refused,
}

/// Compares one recorded call of a ported function with the port.
pub type Check = fn(&Call) -> Outcome;

/// Compares a Lua result with the port's; panics with `context` on mismatch.
#[track_caller]
pub fn same(context: &str, what: &str, lua: &Value, port: i64) {
    let lua_int = exact_int(lua);
    assert_eq!(
        lua_int,
        Some(port),
        "{context}: {what} differs: Lua {lua:?}, port {port}"
    );
}

/// Like [`same`] for values TPF2MP may leave nil.
#[track_caller]
pub fn same_opt(context: &str, what: &str, lua: &Value, port: Option<i64>) {
    match port {
        Some(port) => same(context, what, lua, port),
        None => assert!(
            lua.is_nil(),
            "{context}: {what} differs: Lua {lua:?}, port nil"
        ),
    }
}

/// An integer as the Lua number TPF2MP would hold. Refuses integers that a
/// double cannot hold: mlua would round them silently.
pub fn num(value: i64) -> f64 {
    assert!(
        value.unsigned_abs() <= MAX_EXACT_INTEGER.unsigned_abs(),
        "{value} is not an exact Lua number"
    );
    value as f64
}

/// `Some(n)` as a Lua number, `None` as nil.
pub fn opt(value: Option<i64>) -> Value {
    value.map_or(Value::Nil, |value| Value::Number(num(value)))
}

/// The value as an integer, if it is a whole number.
///
/// mlua reads a Lua 5.1 number as `Value::Integer` exactly when the double
/// is whole and fits `i64`, so the conversion never rounds. Values beyond
/// 2^53 do occur: TPF2MP hands `net * 10` to a clamping add, for one. Each
/// port function then either clamps like Lua or refuses the operand.
///
/// Negative zero is zero. Lua produces it (`-charge` for a zero charge) and
/// treats it as zero everywhere the rules use it; TPF2MP's JSON encoder
/// writes it as `0` so digests agree across platforms.
pub fn exact_int(value: &Value) -> Option<i64> {
    match value {
        Value::Integer(integer) => Some(*integer),
        Value::Number(number) if *number == 0.0 => Some(0),
        _ => None,
    }
}

/// The value as an integer; panics with `what` otherwise.
pub fn int(value: &Value, what: &str) -> i64 {
    exact_int(value)
        .unwrap_or_else(|| panic!("{what}: expected a whole number, Lua gave {value:?}"))
}

/// nil as `None`, otherwise as [`int`].
pub fn opt_int(value: &Value, what: &str) -> Option<i64> {
    (!value.is_nil()).then(|| int(value, what))
}

/// A whole number beyond Lua's exact range that a double still holds
/// exactly, as a Lua number.
pub fn wide_num(value: i64) -> f64 {
    let double = value as f64;
    assert_eq!(double as i64, value, "{value} is not a double");
    double
}

/// Whole numbers up to 2^62 in magnitude that a double holds exactly: every
/// value an operand of a clamping function can take in TPF2MP.
pub fn wide_amount() -> impl proptest::strategy::Strategy<Value = i64> {
    use proptest::strategy::Strategy;
    (-(1i64 << 62)..(1i64 << 62)).prop_map(|value| value as f64 as i64)
}

pub fn get(table: &Table, key: &str) -> Value {
    table.get(key).unwrap()
}

/// Integer field of a table.
pub fn field(table: &Table, key: &str) -> i64 {
    int(&get(table, key), key)
}

/// Optional integer field of a table.
pub fn opt_field(table: &Table, key: &str) -> Option<i64> {
    opt_int(&get(table, key), key)
}

pub fn string(value: &Value, what: &str) -> String {
    match value {
        Value::String(string) => string.to_str().unwrap().to_owned(),
        _ => panic!("{what}: expected a string, Lua gave {value:?}"),
    }
}

pub fn table(value: &Value, what: &str) -> Table {
    match value {
        Value::Table(table) => table.clone(),
        _ => panic!("{what}: expected a table, Lua gave {value:?}"),
    }
}

pub fn opt_table(value: &Value, what: &str) -> Option<Table> {
    (!value.is_nil()).then(|| table(value, what))
}

/// A Lua table keyed by strings with integer values, as a sorted map.
pub fn int_map(table: &Table, what: &str) -> std::collections::BTreeMap<String, i64> {
    table
        .pairs::<Value, Value>()
        .map(|pair| {
            let (key, value) = pair.unwrap();
            (string(&key, what), int(&value, what))
        })
        .collect()
}

/// Property-test runner. Failures print the shrunk input; nothing is written
/// to the source tree.
pub fn runner(cases: u32) -> TestRunner {
    TestRunner::new(Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    })
}
