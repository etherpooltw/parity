// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::path::PathBuf;
use std::sync::Arc;

use dir::default_data_path;
use ethcore::client::{Client, BlockChainClient, BlockId};
use ethcore::transaction::{Transaction, Action};
use ethsync::LightSync;
use futures::{future, IntoFuture, Future, BoxFuture};
use hash_fetch::fetch::Client as FetchClient;
use hash_fetch::urlhint::ContractClient;
use helpers::replace_home;
use light::client::Client as LightClient;
use light::on_demand::{self, OnDemand};
use rpc_apis::SignerService;
use parity_reactor;
use util::{Bytes, Address};

#[derive(Debug, PartialEq, Clone)]
pub struct Configuration {
	pub enabled: bool,
	pub dapps_path: PathBuf,
	pub extra_dapps: Vec<PathBuf>,
}

impl Default for Configuration {
	fn default() -> Self {
		let data_dir = default_data_path();
		Configuration {
			enabled: true,
			dapps_path: replace_home(&data_dir, "$BASE/dapps").into(),
			extra_dapps: vec![],
		}
	}
}

/// Registrar implementation of the full client.
pub struct FullRegistrar {
	/// Handle to the full client.
	pub client: Arc<Client>,
}

impl ContractClient for FullRegistrar {
	fn registrar(&self) -> Result<Address, String> {
		self.client.additional_params().get("registrar")
			 .ok_or_else(|| "Registrar not defined.".into())
			 .and_then(|registrar| {
				 registrar.parse().map_err(|e| format!("Invalid registrar address: {:?}", e))
			 })
	}

	fn call(&self, address: Address, data: Bytes) -> BoxFuture<Bytes, String> {
		self.client.call_contract(BlockId::Latest, address, data)
			.into_future()
			.boxed()
	}
}

/// Registrar implementation for the light client.
pub struct LightRegistrar {
	/// The light client.
	pub client: Arc<LightClient>,
	/// Handle to the on-demand service.
	pub on_demand: Arc<OnDemand>,
	/// Handle to the light network service.
	pub sync: Arc<LightSync>,
}

impl ContractClient for LightRegistrar {
	fn registrar(&self) -> Result<Address, String> {
		self.client.engine().additional_params().get("registrar")
			 .ok_or_else(|| "Registrar not defined.".into())
			 .and_then(|registrar| {
				 registrar.parse().map_err(|e| format!("Invalid registrar address: {:?}", e))
			 })
	}

	fn call(&self, address: Address, data: Bytes) -> BoxFuture<Bytes, String> {
		let (header, env_info) = (self.client.best_block_header(), self.client.latest_env_info());

		let maybe_future = self.sync.with_context(move |ctx| {
			self.on_demand
				.transaction_proof(ctx, on_demand::request::TransactionProof {
					tx: Transaction {
						nonce: self.client.engine().account_start_nonce(),
						action: Action::Call(address),
						gas: 50_000_000.into(),
						gas_price: 0.into(),
						value: 0.into(),
						data: data,
					}.fake_sign(Address::default()),
					header: header,
					env_info: env_info,
					engine: self.client.engine().clone(),
				})
				.then(|res| match res {
					Ok(Ok(executed)) => Ok(executed.output),
					Ok(Err(e)) => Err(format!("Failed to execute transaction: {}", e)),
					Err(_) => Err(format!("On-demand service dropped request unexpectedly.")),
				})
		});

		match maybe_future {
			Some(fut) => fut.boxed(),
			None => future::err("cannot query registry: network disabled".into()).boxed(),
		}
	}
}

// TODO: light client implementation forwarding to OnDemand and waiting for future
// to resolve.
pub struct Dependencies {
	pub sync_status: Arc<SyncStatus>,
	pub contract_client: Arc<ContractClient>,
	pub remote: parity_reactor::TokioRemote,
	pub fetch: FetchClient,
	pub signer: Arc<SignerService>,
}

pub fn new(configuration: Configuration, deps: Dependencies)
	-> Result<Option<Middleware>, String>
{
	if !configuration.enabled {
		return Ok(None);
	}

	dapps_middleware(
		deps,
		configuration.dapps_path,
		configuration.extra_dapps,
	).map(Some)
}

pub use self::server::{SyncStatus, Middleware, dapps_middleware};

#[cfg(not(feature = "dapps"))]
mod server {
	use super::Dependencies;
	use std::path::PathBuf;
	use parity_rpc::{hyper, RequestMiddleware, RequestMiddlewareAction};

	pub type SyncStatus = Fn() -> bool;

	pub struct Middleware;
	impl RequestMiddleware for Middleware {
		fn on_request(
			&self, _req: &hyper::server::Request<hyper::net::HttpStream>, _control: &hyper::Control
		) -> RequestMiddlewareAction {
			unreachable!()
		}
	}

	pub fn dapps_middleware(
		_deps: Dependencies,
		_dapps_path: PathBuf,
		_extra_dapps: Vec<PathBuf>,
	) -> Result<Middleware, String> {
		Err("Your Parity version has been compiled without WebApps support.".into())
	}
}

#[cfg(feature = "dapps")]
mod server {
	use super::Dependencies;
	use std::path::PathBuf;
	use std::sync::Arc;

	use parity_dapps;
	use parity_reactor;

	pub use parity_dapps::Middleware;
	pub use parity_dapps::SyncStatus;

	pub fn dapps_middleware(
		deps: Dependencies,
		dapps_path: PathBuf,
		extra_dapps: Vec<PathBuf>,
	) -> Result<Middleware, String> {
		let signer = deps.signer.clone();
		let parity_remote = parity_reactor::Remote::new(deps.remote.clone());
		let web_proxy_tokens = Arc::new(move |token| signer.is_valid_web_proxy_access_token(&token));

		Ok(parity_dapps::Middleware::new(
			parity_remote,
			deps.signer.address(),
			dapps_path,
			extra_dapps,
			deps.contract_client,
			deps.sync_status,
			web_proxy_tokens,
			deps.fetch.clone(),
		))
	}
}
