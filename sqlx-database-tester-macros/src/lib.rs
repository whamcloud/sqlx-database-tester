//! macros for sqlx-database-tester

use darling::{ast::NestedMeta, FromMeta};
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::Ident;
mod generators;

/// Pool configuration
#[derive(Debug, FromMeta)]
pub(crate) struct Pool {
	/// The variable with pool that will be exposed to the test function
	variable: Ident,
	/// The optional transaction variable
	#[darling(default)]
	transaction_variable: Option<Ident>,
	/// The migration directory path
	#[darling(default)]
	migrations: Option<String>,
	/// Should the migration be skipped
	#[darling(default)]
	skip_migrations: bool,
}

impl Pool {
	/// Return identifier for variable that will contain the database name
	fn database_name_var(&self) -> Ident {
		format_ident!("__{}_db_name", &self.variable)
	}
}

/// Test case configuration
#[derive(Debug, FromMeta)]
pub(crate) struct MacroArgs {
	/// Sqlx log level
	#[darling(default)]
	level: String,
	/// The variable the database pool will be exposed in
	#[darling(multiple)]
	pool: Vec<Pool>,
}

/// Marks async test function that exposes database pool to its scope
///
/// ## Macro attributes:
///
/// - `variable`: Variable of the PgPool to be exposed to the function scope
///   (mandatory)
/// - `other_dir_migrations`: Path to SQLX other_dir_migrations directory for
///   the specified pool (falls back to default ./migrations directory if left
///   out)
/// - `skip_migrations`: If present, doesn't run any other_dir_migrations
/// ```
/// #[sqlx_database_tester::test(
/// 	pool(variable = "default_migrated_pool"),
/// 	pool(
/// 		variable = "migrated_pool",
/// 		migrations = "./other_dir_migrations"
/// 	),
/// 	pool(
/// 		variable = "empty_db_pool",
/// 		transaction_variable = "empty_db_transaction",
/// 		skip_migrations
/// 	)
/// )]
/// async fn test_server_sta_rt() {
/// 	let migrated_pool_tables =
/// 		sqlx::query!("SELECT * FROM pg_catalog.pg_tables")
/// 			.fetch_all(&migrated_pool)
/// 			.await
/// 			.unwrap();
/// 	let empty_pool_tables =
/// 		sqlx::query!("SELECT * FROM pg_catalog.pg_tables")
/// 			.fetch_all(&migrated_pool)
/// 			.await
/// 			.unwrap();
/// 	println!("Migrated pool tables: \n {:#?}", migrated_pool_tables);
/// 	println!("Empty pool tables: \n {:#?}", empty_pool_tables);
/// }
/// ```
#[proc_macro_attribute]
pub fn test(test_attr: TokenStream, item: TokenStream) -> TokenStream {
	// Retype to proc-macro2 types
	let test_attr = proc_macro2::TokenStream::from(test_attr);

	// Darling internal format
	let nested_meta = match NestedMeta::parse_meta_list(test_attr) {
		Ok(v) => v,
		Err(e) => {
			return TokenStream::from(e.to_compile_error());
		}
	};

	let macro_args = match MacroArgs::from_list(&nested_meta) {
		Ok(args) => args,
		Err(e) => {
			return TokenStream::from(e.write_errors());
		}
	};

	let level = macro_args.level.as_str();

	let mut input = syn::parse_macro_input!(item as syn::ItemFn);
	let attrs = &input.attrs;
	let vis = &input.vis;
	let sig = &mut input.sig;
	let body = &input.block;

	let Some(runtime) = generators::runtime() else {
		return syn::Error::new(
			Span::call_site(),
			"One of 'runtime-actix' and 'runtime-tokio' features needs to be enabled",
		)
		.into_compile_error()
		.into();
	};

	if sig.asyncness.is_none() {
		return syn::Error::new_spanned(
			input.sig.fn_token,
			"the async keyword is missing from the function declaration",
		)
		.to_compile_error()
		.into();
	}

	sig.asyncness = None;

	let database_name_vars = generators::database_name_vars(&macro_args);
	let database_creators = generators::database_creators(&macro_args);
	let database_migrations_exposures = generators::database_migrations_exposures(&macro_args);
	let database_closers = generators::database_closers(&macro_args);
	let database_destructors = generators::database_destructors(&macro_args);
	let sleep = generators::sleep();

	(quote! {
		#[::core::prelude::v1::test]
		#(#attrs)*
		#vis #sig {
			/// Maximum number of tries to attempt database connection
			const MAX_RETRIES: u8 = 30;
			/// Time between retries, in seconds
			const TIME_BETWEEN_RETRIES: u64 = 10;

			#[allow(clippy::expect_used)]
			async fn connect_with_retry() -> Result<sqlx::PgPool, sqlx::Error> {
				let mut i = 0;
				loop {
					let db_pool = sqlx::PgPool::connect_with(sqlx_database_tester::connect_options(
						sqlx_database_tester::derive_db_prefix(&sqlx_database_tester::get_database_uri())
							.expect("Getting database name")
							.as_deref()
							.unwrap_or_default(),
						#level,
					))
					.await;
					match db_pool {
						Ok(pool) => break Ok(pool),
						Err(e) => {
							if i >= MAX_RETRIES {
								break Err(e);
							}
						}
					}
					#sleep(std::time::Duration::from_secs(TIME_BETWEEN_RETRIES)).await;
					i += 1;
				}
			}

			sqlx_database_tester::dotenv::dotenv().ok();
			#(#database_name_vars)*
			#runtime.block_on(async {
				#[allow(clippy::expect_used)]
				let db_pool = connect_with_retry().await.expect("connecting to db for creation");
				#(#database_creators)*
			});

			let result = std::panic::catch_unwind(|| {
				#runtime.block_on(async {
					#(#database_migrations_exposures)*
					let res = #body;
					#(#database_closers)*
					res
				})
			});

			#runtime.block_on(async {
				#[allow(clippy::expect_used)]
				let db_pool = connect_with_retry().await.expect("connecting to db for deletion");
				#(#database_destructors)*
			});

			match result {
				std::result::Result::Err(_) => std::panic!("The main test function crashed, the test database got cleaned"),
				std::result::Result::Ok(o) => o
			}
		}
	}).into()
}
