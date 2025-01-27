use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use ruma::{
	api::{
		client::error::ErrorKind,
		federation::directory::{get_public_rooms, get_public_rooms_filtered},
	},
	directory::Filter,
};

use crate::{Error, Result, Ruma};

/// # `POST /_matrix/federation/v1/publicRooms`
///
/// Lists the public rooms on this server.
#[tracing::instrument(name = "publicrooms", level = "debug", skip_all, fields(%client))]
pub(crate) async fn get_public_rooms_filtered_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_public_rooms_filtered::v1::Request>,
) -> Result<get_public_rooms_filtered::v1::Response> {
	if !services
		.server
		.config
		.allow_public_room_directory_over_federation
	{
		return Err(Error::BadRequest(ErrorKind::forbidden(), "Room directory is not public"));
	}

	let response = crate::client::get_public_rooms_filtered_helper(
		&services,
		None,
		body.limit,
		body.since.as_deref(),
		&body.filter,
		&body.room_network,
	)
	.await
	.map_err(|_| {
		Error::BadRequest(ErrorKind::Unknown, "Failed to return this server's public room list.")
	})?;

	Ok(get_public_rooms_filtered::v1::Response {
		chunk: response.chunk,
		prev_batch: response.prev_batch,
		next_batch: response.next_batch,
		total_room_count_estimate: response.total_room_count_estimate,
	})
}

/// # `GET /_matrix/federation/v1/publicRooms`
///
/// Lists the public rooms on this server.
#[tracing::instrument(name = "publicrooms", level = "debug", skip_all, fields(%client))]
pub(crate) async fn get_public_rooms_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_public_rooms::v1::Request>,
) -> Result<get_public_rooms::v1::Response> {
	if !services
		.globals
		.allow_public_room_directory_over_federation()
	{
		return Err(Error::BadRequest(ErrorKind::forbidden(), "Room directory is not public"));
	}

	let response = crate::client::get_public_rooms_filtered_helper(
		&services,
		None,
		body.limit,
		body.since.as_deref(),
		&Filter::default(),
		&body.room_network,
	)
	.await
	.map_err(|_| {
		Error::BadRequest(ErrorKind::Unknown, "Failed to return this server's public room list.")
	})?;

	Ok(get_public_rooms::v1::Response {
		chunk: response.chunk,
		prev_batch: response.prev_batch,
		next_batch: response.next_batch,
		total_room_count_estimate: response.total_room_count_estimate,
	})
}
