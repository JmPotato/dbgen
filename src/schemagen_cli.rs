//! CLI driver of `dbschemagen`.

use crate::parser::QName;
use failure::Error;
use rand::{
    distributions::{Distribution, Pareto, Triangular, Uniform},
    seq::SliceRandom,
    thread_rng, Rng, RngCore,
};
use std::{
    collections::{BTreeSet, HashSet},
    fmt::Write,
    iter::repeat_with,
    str::FromStr,
};
use structopt::StructOpt;

/// Arguments to the `dbschemagen` CLI program.
#[derive(StructOpt, Debug)]
#[structopt(raw(setting = "structopt::clap::AppSettings::TrailingVarArg"))]
pub struct Args {
    /// Schema name.
    #[structopt(short = "s", long = "schema-name", help = "Schema name")]
    pub schema_name: String,

    /// Estimated total database dump size in bytes.
    #[structopt(short = "z", long = "size", help = "Estimated total database dump size in bytes")]
    pub size: f64,

    /// Number of tables to generate
    #[structopt(short = "t", long = "tables-count", help = "Number of tables to generate")]
    pub tables_count: u32,

    /// SQL dialect.
    #[structopt(
        short = "d",
        long = "dialect",
        help = "SQL dialect",
        raw(possible_values = r#"&["mysql", "postgresql", "sqlite"]"#)
    )]
    pub dialect: Dialect,

    /// Number of INSERT statements per file.
    #[structopt(
        short = "n",
        long = "inserts-count",
        help = "Number of INSERT statements per file",
        default_value = "1000"
    )]
    pub inserts_count: u64,

    /// Number of rows per INSERT statement.
    #[structopt(
        short = "r",
        long = "rows-count",
        help = "Number of rows per INSERT statement",
        default_value = "100"
    )]
    pub rows_count: u64,

    /// Additional arguments passed to every `dbgen` invocation
    #[structopt(help = "Additional arguments passed to every `dbgen` invocation")]
    pub args: Vec<String>,
}

/// The SQL dialect used when generating the schemas.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Dialect {
    /// MySQL dialect.
    MySQL,
    /// PostgreSQL dialect.
    PostgreSQL,
    /// SQLite dialect.
    SQLite,
}

impl FromStr for Dialect {
    type Err = Error;
    fn from_str(dialect: &str) -> Result<Self, Self::Err> {
        Ok(match dialect {
            "mysql" => Dialect::MySQL,
            "postgresql" => Dialect::PostgreSQL,
            "sqlite" => Dialect::SQLite,
            _ => failure::bail!("Unsupported SQL dialect {}", dialect),
        })
    }
}

struct Column {
    /// The column type.
    ty: String,
    /// `dbgen` expression to generate a value of this type.
    expr: String,
    /// The -log₂(probability) which two randomly generated values will collide (assuming perfect RNG).
    neg_log2_prob: f64,
    /// The estimated average formatted length a generated value of this column.
    average_len: f64,
}

type ColumnGenerator = fn(Dialect, &mut dyn RngCore) -> Column;

#[allow(clippy::cast_precision_loss)]
fn gen_int_column(dialect: Dialect, rng: &mut dyn RngCore) -> Column {
    let bytes = rng.gen_range(0, 8);
    let unsigned = rng.gen::<bool>();
    #[allow(clippy::match_same_arms)]
    let ty = match (dialect, unsigned, bytes) {
        (Dialect::MySQL, false, 0) => "tinyint",
        (Dialect::MySQL, false, 1) => "smallint",
        (Dialect::MySQL, false, 2) => "mediumint",
        (Dialect::MySQL, false, 3) => "int",
        (Dialect::MySQL, false, _) => "bigint",
        (Dialect::MySQL, true, 0) => "tinyint unsigned",
        (Dialect::MySQL, true, 1) => "smallint unsigned",
        (Dialect::MySQL, true, 2) => "mediumint unsigned",
        (Dialect::MySQL, true, 3) => "int unsigned",
        (Dialect::MySQL, true, _) => "bigint unsigned",
        (Dialect::PostgreSQL, false, 0..=1) => "smallint",
        (Dialect::PostgreSQL, false, 2..=3) => "int",
        (Dialect::PostgreSQL, false, _) => "bigint",
        (Dialect::PostgreSQL, true, 0) => "smallint",
        (Dialect::PostgreSQL, true, 1..=2) => "int",
        (Dialect::PostgreSQL, true, 3..=6) => "bigint",
        (Dialect::PostgreSQL, true, _) => "numeric(20)",
        (Dialect::SQLite, _, _) => "integer",
    };
    let ty = format!("{} not null", ty);
    let (min, max) = if unsigned {
        (0, (256 << (8 * bytes)) - 1)
    } else {
        let base: i128 = 128 << (8 * bytes);
        (-base, base - 1)
    };
    let neg_log2_prob = f64::from(bytes) * 8.0;

    let end = (max + 1) as f64;
    let digits = end.log10().ceil();
    let mut average_len = digits - (10_f64.powf(digits) - 10.0) / (9.0 * end);
    if !unsigned {
        average_len = average_len * 2.0 + 1.0;
    }

    Column {
        ty,
        expr: format!("rand.range_inclusive({}, {})", min, max),
        neg_log2_prob,
        average_len,
    }
}

fn gen_serial_column(dialect: Dialect, _: &mut dyn RngCore) -> Column {
    let ty = match dialect {
        Dialect::MySQL => "bigint unsigned not null",
        Dialect::PostgreSQL => "bigserial",
        Dialect::SQLite => "integer not null",
    };
    Column {
        ty: ty.to_owned(),
        expr: "rownum".to_owned(),
        neg_log2_prob: 64.0,
        average_len: 6.0,
    }
}

const LOG2_10: f64 = 3.321_928_094_887_362;

#[allow(clippy::cast_precision_loss)]
fn gen_decimal_column(_: Dialect, rng: &mut dyn RngCore) -> Column {
    let before = rng.gen_range(1, 19);
    let after = rng.gen_range(0, 31);
    let limit = "9".repeat(before);
    Column {
        ty: format!("decimal({}, {}) not null", before + after, after),
        expr: format!(
            "rand.range_inclusive(-{0}, {0}) || rand.regex('\\.[0-9]{{{1}}}')",
            limit, after
        ),
        neg_log2_prob: LOG2_10 * (before + after) as f64 + 1.0,
        average_len: (before + after) as f64 + 17.0 / 9.0,
    }
}

// 4382594 / 1112064
const AVERAGE_LEN_PER_CHAR: f64 = 3.940_954_837_131_676;
const VALID_CHARS_COUNT: f64 = 1_112_064.0;

fn gen_varchar_column(_: Dialect, rng: &mut dyn RngCore) -> Column {
    let len = rng.gen_range(1, 255);
    let residue = (VALID_CHARS_COUNT / (VALID_CHARS_COUNT - 1.0)).log2();
    Column {
        ty: format!("varchar({}) not null", len),
        expr: format!("rand.regex('.{{0,{}}}', 's')", len),
        neg_log2_prob: f64::from(len + 1).log2() - residue,
        average_len: AVERAGE_LEN_PER_CHAR * 0.5 * f64::from(len) + 2.0,
    }
}

fn gen_char_column(_: Dialect, rng: &mut dyn RngCore) -> Column {
    let len = rng.gen_range(1, 255);
    let factor = VALID_CHARS_COUNT.log2();
    Column {
        ty: format!("char({}) not null", len),
        expr: format!("rand.regex('.{{{}}}', 's')", len),
        neg_log2_prob: factor * f64::from(len),
        average_len: AVERAGE_LEN_PER_CHAR * f64::from(len) + 2.0,
    }
}

fn gen_timestamp_column(dialect: Dialect, _: &mut dyn RngCore) -> Column {
    let ty = match dialect {
        Dialect::SQLite => "text not null",
        Dialect::MySQL | Dialect::PostgreSQL => "timestamp not null",
    };
    Column {
        ty: ty.to_owned(),
        expr: "TIMESTAMP '1970-01-01 00:00:00' + INTERVAL rand.range_inclusive(0, 2147483647) SECOND".to_owned(),
        neg_log2_prob: 31.0,
        average_len: 21.0,
    }
}

const DATEIME_SECONDS: f64 = 284_012_524_800_f64;

fn gen_datetime_column(dialect: Dialect, _: &mut dyn RngCore) -> Column {
    let ty = match dialect {
        Dialect::SQLite => "text not null",
        Dialect::MySQL => "datetime not null",
        Dialect::PostgreSQL => "timestamp not null",
    };
    Column {
        ty: ty.to_owned(),
        expr: "TIMESTAMP '1000-01-01 00:00:00' + INTERVAL rand.range(0, 284012524800) SECOND".to_owned(),
        neg_log2_prob: DATEIME_SECONDS.log2(),
        average_len: 21.0,
    }
}

fn gen_nullable_bool(_: Dialect, rng: &mut dyn RngCore) -> Column {
    let p = rng.gen::<f64>();
    Column {
        ty: "boolean".to_owned(),
        expr: format!("CASE rand.bool({}) WHEN TRUE THEN '' || rand.bool(0.5) END", p),
        neg_log2_prob: -((1.5 * p - 2.0) * p + 1.0).log2(),
        average_len: 4.0 - p,
    }
}

static GENERATORS: [ColumnGenerator; 8] = [
    gen_int_column,
    gen_serial_column,
    gen_varchar_column,
    gen_char_column,
    gen_timestamp_column,
    gen_datetime_column,
    gen_nullable_bool,
    gen_decimal_column,
];

fn gen_column(dialect: Dialect, rng: &mut dyn RngCore) -> Column {
    let gen = GENERATORS.choose(rng).unwrap();
    gen(dialect, rng)
}

#[allow(clippy::cast_possible_truncation)]
fn append_index(
    schema: &mut String,
    index_sets: &mut HashSet<BTreeSet<usize>>,
    dialect: Dialect,
    rng: &mut dyn RngCore,
    columns: &[Column],
    unique_cutoff: f64,
    is_primary_key: bool,
) {
    // randomly generate an index set
    let mut index_set = BTreeSet::new();
    let dist = Uniform::new(0, columns.len());

    for i in 0..(columns.len() as u32) {
        if rng.gen_ratio(1, 2 + i) {
            index_set.insert(dist.sample(rng));
        }
    }
    let total_neg_log2_prob: f64 = index_set.iter().map(|i| columns[*i].neg_log2_prob).sum();
    let is_unique = total_neg_log2_prob > unique_cutoff;

    if is_primary_key && !is_unique {
        return;
    }

    let index_spec = index_set
        .iter()
        .map(|i| format!("c{}", i))
        .collect::<Vec<_>>()
        .join(", ");

    if index_set.is_empty() || !index_sets.insert(index_set) {
        return;
    }

    if is_primary_key {
        schema.push_str(",\nPRIMARY KEY (");
    } else if is_unique {
        schema.push_str(",\nUNIQUE (");
    } else if dialect == Dialect::MySQL {
        schema.push_str(",\nKEY (");
    } else {
        return;
    }
    schema.push_str(&index_spec);
    schema.push(')');
}

struct Table {
    schema: String,
    rows_count: u64,
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn gen_table(dialect: Dialect, rng: &mut dyn RngCore, target_size: f64) -> Table {
    let mut schema = String::from("CREATE TABLE _ (\n");

    let columns_count = Triangular::new(1.0, 50.0, 6.0).sample(rng) as usize;
    let columns = {
        let rng2 = &mut *rng;
        repeat_with(move || gen_column(dialect, rng2))
            .take(columns_count)
            .collect::<Vec<_>>()
    };

    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            schema.push_str(",\n");
        }
        write!(&mut schema, "c{} {} {{{{{}}}}}", i, col.ty, col.expr).unwrap();
    }

    let average_len_per_row: f64 = columns.iter().map(|col| col.average_len + 2.0).sum();
    let rows_count = (target_size / average_len_per_row).ceil();

    // if the collision probability of 2 rows is p,
    // the collision probability of N rows is approximately 1 - exp(-0.5 p N^2) for large N and small p.
    // if a column is going to be marked a UNIQUE, we want this probability be less than ~0.005,
    // i.e. -log2(p) > 2log2(N) - log2(~0.01).
    let unique_cutoff = rows_count.log2() * 2.0 + 6.736_593_289_427_474;

    // pick a random column as primary key
    let mut index_sets = HashSet::new();
    append_index(
        &mut schema,
        &mut index_sets,
        dialect,
        rng,
        &columns,
        unique_cutoff,
        true,
    );
    while rng.gen_ratio(columns_count as u32, (columns_count + index_sets.len()) as u32) {
        append_index(
            &mut schema,
            &mut index_sets,
            dialect,
            rng,
            &columns,
            unique_cutoff,
            false,
        );
    }
    schema.push_str("\n)");

    Table {
        schema,
        rows_count: (rows_count as u64).max(1),
    }
}

fn gen_tables<'a>(
    dialect: Dialect,
    mut rng: impl Rng + 'a,
    total_target_size: f64,
    tables_count: u32,
) -> impl Iterator<Item = Table> + 'a {
    let distr = Pareto::new(1.0, 1.16);
    let relative_sizes = distr
        .sample_iter(&mut rng)
        .take(tables_count as usize)
        .collect::<Vec<_>>();
    let total_relative_size: f64 = relative_sizes.iter().sum();
    let ratio = total_target_size / total_relative_size;
    relative_sizes
        .into_iter()
        .map(move |f| gen_table(dialect, &mut rng, f * ratio))
}

/// Generates a shell script for invoking `dbgen` into stdout.
pub fn print_script(args: &Args) {
    let schema_name = QName::parse(&args.schema_name).expect("invalid schema name");
    let quoted_schema_name = shlex::quote(&args.schema_name);

    println!(
        "#!/bin/sh\nset -eu\necho 'CREATE SCHEMA '{}';' > {}-schema-create.sql\n",
        quoted_schema_name,
        schema_name.unique_name(),
    );
    let extra_args = args.args.iter().map(|s| shlex::quote(s)).collect::<Vec<_>>().join(" ");
    let rows_count_per_file = args.rows_count * args.inserts_count;
    for (i, table) in gen_tables(args.dialect, thread_rng(), args.size, args.tables_count).enumerate() {
        let mut files = (table.rows_count / rows_count_per_file) + 1;
        let residue = table.rows_count % rows_count_per_file;
        let (last_inserts, last_rows) = if residue == 0 {
            files -= 1;
            (args.inserts_count, args.rows_count)
        } else {
            let inserts = residue / args.rows_count;
            let rows_residue = residue % args.rows_count;
            if rows_residue == 0 {
                (inserts, args.rows_count)
            } else {
                (inserts + 1, rows_residue)
            }
        };
        println!(
            "dbgen -i /dev/stdin -o . -t {}.s{} -n {} -r {} -k {} \
             --last-file-inserts-count {} --last-insert-rows-count {} \
             {} <<SCHEMAEOF\n{}\nSCHEMAEOF\n",
            quoted_schema_name,
            i,
            args.inserts_count,
            args.rows_count,
            files,
            last_inserts,
            last_rows,
            extra_args,
            table.schema,
        );
    }
}
