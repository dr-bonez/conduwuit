use std::{collections::BTreeMap, net::IpAddr, time::Instant};

use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use conduwuit::{
	debug, debug_warn, err, error,
	result::LogErr,
	trace,
	utils::{
		stream::{automatic_width, BroadbandExt, TryBroadbandExt},
		IterStream, ReadyExt,
	},
	warn, Err, Error, Result,
};
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt};
use itertools::Itertools;
use ruma::{
	api::{
		client::error::ErrorKind,
		federation::transactions::{
			edu::{
				DeviceListUpdateContent, DirectDeviceContent, Edu, PresenceContent,
				ReceiptContent, SigningKeyUpdateContent, TypingContent,
			},
			send_transaction_message,
		},
	},
	events::receipt::{ReceiptEvent, ReceiptEventContent, ReceiptType},
	to_device::DeviceIdOrAllDevices,
	CanonicalJsonObject, OwnedEventId, OwnedRoomId, ServerName,
};
use service::{
	sending::{EDU_LIMIT, PDU_LIMIT},
	Services,
};

use crate::{
	utils::{self},
	Ruma,
};

type ResolvedMap = BTreeMap<OwnedEventId, Result>;
type Pdu = (OwnedRoomId, OwnedEventId, CanonicalJsonObject);

/// # `PUT /_matrix/federation/v1/send/{txnId}`
///
/// Push EDUs and PDUs to this server.
#[tracing::instrument(
	name = "send",
	level = "debug",
	skip_all,
	fields(
		%client,
		origin = body.origin().as_str()
	),
)]
pub(crate) async fn send_transaction_message_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<send_transaction_message::v1::Request>,
) -> Result<send_transaction_message::v1::Response> {
	if body.origin() != body.body.origin {
		return Err!(Request(Forbidden(
			"Not allowed to send transactions on behalf of other servers"
		)));
	}

	if body.pdus.len() > PDU_LIMIT {
		return Err!(Request(Forbidden(
			"Not allowed to send more than {PDU_LIMIT} PDUs in one transaction"
		)));
	}

	if body.edus.len() > EDU_LIMIT {
		return Err!(Request(Forbidden(
			"Not allowed to send more than {EDU_LIMIT} EDUs in one transaction"
		)));
	}

	let txn_start_time = Instant::now();
	trace!(
		pdus = body.pdus.len(),
		edus = body.edus.len(),
		elapsed = ?txn_start_time.elapsed(),
		id = ?body.transaction_id,
		origin =?body.origin(),
		"Starting txn",
	);

	let pdus = body
		.pdus
		.iter()
		.stream()
		.broad_then(|pdu| services.rooms.event_handler.parse_incoming_pdu(pdu))
		.inspect_err(|e| debug_warn!("Could not parse PDU: {e}"))
		.ready_filter_map(Result::ok);

	let edus = body
		.edus
		.iter()
		.map(|edu| edu.json().get())
		.map(serde_json::from_str)
		.filter_map(Result::ok)
		.stream();

	let results = handle(&services, &client, body.origin(), txn_start_time, pdus, edus).await?;

	debug!(
		pdus = body.pdus.len(),
		edus = body.edus.len(),
		elapsed = ?txn_start_time.elapsed(),
		id = ?body.transaction_id,
		origin =?body.origin(),
		"Finished txn",
	);
	for (id, result) in &results {
		if let Err(e) = result {
			if matches!(e, Error::BadRequest(ErrorKind::NotFound, _)) {
				warn!("Incoming PDU failed {id}: {e:?}");
			}
		}
	}

	Ok(send_transaction_message::v1::Response {
		pdus: results
			.into_iter()
			.map(|(e, r)| (e, r.map_err(error::sanitized_message)))
			.collect(),
	})
}

async fn handle(
	services: &Services,
	client: &IpAddr,
	origin: &ServerName,
	started: Instant,
	pdus: impl Stream<Item = Pdu> + Send,
	edus: impl Stream<Item = Edu> + Send,
) -> Result<ResolvedMap> {
	// group pdus by room
	let pdus = pdus
		.collect()
		.map(|mut pdus: Vec<_>| {
			pdus.sort_by(|(room_a, ..), (room_b, ..)| room_a.cmp(room_b));
			pdus.into_iter()
				.into_grouping_map_by(|(room_id, ..)| room_id.clone())
				.collect()
		})
		.await;

	// we can evaluate rooms concurrently
	let results: ResolvedMap = pdus
		.into_iter()
		.try_stream()
		.broad_and_then(|(room_id, pdus)| {
			handle_room(services, client, origin, started, room_id, pdus)
				.map_ok(Vec::into_iter)
				.map_ok(IterStream::try_stream)
		})
		.try_flatten()
		.try_collect()
		.boxed()
		.await?;

	// evaluate edus after pdus, at least for now.
	edus.for_each_concurrent(automatic_width(), |edu| handle_edu(services, client, origin, edu))
		.boxed()
		.await;

	Ok(results)
}

async fn handle_room(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	txn_start_time: Instant,
	room_id: OwnedRoomId,
	pdus: Vec<Pdu>,
) -> Result<Vec<(OwnedEventId, Result)>> {
	let _room_lock = services
		.rooms
		.event_handler
		.mutex_federation
		.lock(&room_id)
		.await;

	let mut results = Vec::with_capacity(pdus.len());
	for (_, event_id, value) in pdus {
		services.server.check_running()?;
		let pdu_start_time = Instant::now();
		let result = services
			.rooms
			.event_handler
			.handle_incoming_pdu(origin, &room_id, &event_id, value, true)
			.await
			.map(|_| ());

		debug!(
			pdu_elapsed = ?pdu_start_time.elapsed(),
			txn_elapsed = ?txn_start_time.elapsed(),
			"Finished PDU {event_id}",
		);

		results.push((event_id, result));
	}

	Ok(results)
}

async fn handle_edu(services: &Services, client: &IpAddr, origin: &ServerName, edu: Edu) {
	match edu {
		| Edu::Presence(presence) => {
			handle_edu_presence(services, client, origin, presence).await;
		},
		| Edu::Receipt(receipt) => handle_edu_receipt(services, client, origin, receipt).await,
		| Edu::Typing(typing) => handle_edu_typing(services, client, origin, typing).await,
		| Edu::DeviceListUpdate(content) => {
			handle_edu_device_list_update(services, client, origin, content).await;
		},
		| Edu::DirectToDevice(content) => {
			handle_edu_direct_to_device(services, client, origin, content).await;
		},
		| Edu::SigningKeyUpdate(content) => {
			handle_edu_signing_key_update(services, client, origin, content).await;
		},
		| Edu::_Custom(ref _custom) => {
			debug_warn!(?edu, "received custom/unknown EDU");
		},
	}
}

async fn handle_edu_presence(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	presence: PresenceContent,
) {
	if !services.globals.allow_incoming_presence() {
		return;
	}

	for update in presence.push {
		if update.user_id.server_name() != origin {
			debug_warn!(
				%update.user_id, %origin,
				"received presence EDU for user not belonging to origin"
			);
			continue;
		}

		services
			.presence
			.set_presence(
				&update.user_id,
				&update.presence,
				Some(update.currently_active),
				Some(update.last_active_ago),
				update.status_msg.clone(),
			)
			.await
			.log_err()
			.ok();
	}
}

async fn handle_edu_receipt(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	receipt: ReceiptContent,
) {
	if !services.globals.allow_incoming_read_receipts() {
		return;
	}

	for (room_id, room_updates) in receipt.receipts {
		if services
			.rooms
			.event_handler
			.acl_check(origin, &room_id)
			.await
			.is_err()
		{
			debug_warn!(
				%origin, %room_id,
				"received read receipt EDU from ACL'd server"
			);
			continue;
		}

		for (user_id, user_updates) in room_updates.read {
			if user_id.server_name() != origin {
				debug_warn!(
					%user_id, %origin,
					"received read receipt EDU for user not belonging to origin"
				);
				continue;
			}

			if services
				.rooms
				.state_cache
				.room_members(&room_id)
				.ready_any(|member| member.server_name() == user_id.server_name())
				.await
			{
				for event_id in &user_updates.event_ids {
					let user_receipts =
						BTreeMap::from([(user_id.clone(), user_updates.data.clone())]);
					let receipts = BTreeMap::from([(ReceiptType::Read, user_receipts)]);
					let receipt_content = BTreeMap::from([(event_id.to_owned(), receipts)]);
					let event = ReceiptEvent {
						content: ReceiptEventContent(receipt_content),
						room_id: room_id.clone(),
					};

					services
						.rooms
						.read_receipt
						.readreceipt_update(&user_id, &room_id, &event)
						.await;
				}
			} else {
				debug_warn!(
					%user_id, %room_id, %origin,
					"received read receipt EDU from server who does not have a member in the room",
				);
				continue;
			}
		}
	}
}

async fn handle_edu_typing(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	typing: TypingContent,
) {
	if !services.server.config.allow_incoming_typing {
		return;
	}

	if typing.user_id.server_name() != origin {
		debug_warn!(
			%typing.user_id, %origin,
			"received typing EDU for user not belonging to origin"
		);
		return;
	}

	if services
		.rooms
		.event_handler
		.acl_check(typing.user_id.server_name(), &typing.room_id)
		.await
		.is_err()
	{
		debug_warn!(
			%typing.user_id, %typing.room_id, %origin,
			"received typing EDU for ACL'd user's server"
		);
		return;
	}

	if services
		.rooms
		.state_cache
		.is_joined(&typing.user_id, &typing.room_id)
		.await
	{
		if typing.typing {
			let timeout = utils::millis_since_unix_epoch().saturating_add(
				services
					.server
					.config
					.typing_federation_timeout_s
					.saturating_mul(1000),
			);
			services
				.rooms
				.typing
				.typing_add(&typing.user_id, &typing.room_id, timeout)
				.await
				.log_err()
				.ok();
		} else {
			services
				.rooms
				.typing
				.typing_remove(&typing.user_id, &typing.room_id)
				.await
				.log_err()
				.ok();
		}
	} else {
		debug_warn!(
			%typing.user_id, %typing.room_id, %origin,
			"received typing EDU for user not in room"
		);
	}
}

async fn handle_edu_device_list_update(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	content: DeviceListUpdateContent,
) {
	let DeviceListUpdateContent { user_id, .. } = content;

	if user_id.server_name() != origin {
		debug_warn!(
			%user_id, %origin,
			"received device list update EDU for user not belonging to origin"
		);
		return;
	}

	services.users.mark_device_key_update(&user_id).await;
}

async fn handle_edu_direct_to_device(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	content: DirectDeviceContent,
) {
	let DirectDeviceContent { sender, ev_type, message_id, messages } = content;

	if sender.server_name() != origin {
		debug_warn!(
			%sender, %origin,
			"received direct to device EDU for user not belonging to origin"
		);
		return;
	}

	// Check if this is a new transaction id
	if services
		.transaction_ids
		.existing_txnid(&sender, None, &message_id)
		.await
		.is_ok()
	{
		return;
	}

	for (target_user_id, map) in &messages {
		for (target_device_id_maybe, event) in map {
			let Ok(event) = event.deserialize_as().map_err(|e| {
				err!(Request(InvalidParam(error!("To-Device event is invalid: {e}"))))
			}) else {
				continue;
			};

			let ev_type = ev_type.to_string();
			match target_device_id_maybe {
				| DeviceIdOrAllDevices::DeviceId(target_device_id) => {
					services
						.users
						.add_to_device_event(
							&sender,
							target_user_id,
							target_device_id,
							&ev_type,
							event,
						)
						.await;
				},

				| DeviceIdOrAllDevices::AllDevices => {
					let (sender, ev_type, event) = (&sender, &ev_type, &event);
					services
						.users
						.all_device_ids(target_user_id)
						.for_each(|target_device_id| {
							services.users.add_to_device_event(
								sender,
								target_user_id,
								target_device_id,
								ev_type,
								event.clone(),
							)
						})
						.await;
				},
			}
		}
	}

	// Save transaction id with empty data
	services
		.transaction_ids
		.add_txnid(&sender, None, &message_id, &[]);
}

async fn handle_edu_signing_key_update(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	content: SigningKeyUpdateContent,
) {
	let SigningKeyUpdateContent { user_id, master_key, self_signing_key } = content;

	if user_id.server_name() != origin {
		debug_warn!(
			%user_id, %origin,
			"received signing key update EDU from server that does not belong to user's server"
		);
		return;
	}

	if let Some(master_key) = master_key {
		services
			.users
			.add_cross_signing_keys(&user_id, &master_key, &self_signing_key, &None, true)
			.await
			.log_err()
			.ok();
	}
}
