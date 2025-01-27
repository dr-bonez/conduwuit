use std::{
	borrow::Borrow,
	collections::HashMap,
	fmt::Write,
	sync::{Arc, Mutex as StdMutex, Mutex},
};

use conduwuit::{
	at, err, error,
	pdu::PduBuilder,
	utils,
	utils::{
		math::{usize_from_f64, Expected},
		stream::BroadbandExt,
		IterStream, ReadyExt,
	},
	Err, Error, PduEvent, Result,
};
use database::{Deserialized, Map};
use futures::{future::try_join, FutureExt, StreamExt, TryFutureExt};
use lru_cache::LruCache;
use ruma::{
	events::{
		room::{
			avatar::RoomAvatarEventContent,
			canonical_alias::RoomCanonicalAliasEventContent,
			create::RoomCreateEventContent,
			encryption::RoomEncryptionEventContent,
			guest_access::{GuestAccess, RoomGuestAccessEventContent},
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			join_rules::{AllowRule, JoinRule, RoomJoinRulesEventContent, RoomMembership},
			member::{MembershipState, RoomMemberEventContent},
			name::RoomNameEventContent,
			power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
			topic::RoomTopicEventContent,
		},
		StateEventType, TimelineEventType,
	},
	room::RoomType,
	space::SpaceRoomJoinRule,
	EventEncryptionAlgorithm, EventId, JsOption, OwnedEventId, OwnedRoomAliasId, OwnedRoomId,
	OwnedServerName, OwnedUserId, RoomId, ServerName, UserId,
};
use serde::Deserialize;

use crate::{
	rooms,
	rooms::{
		short::{ShortEventId, ShortStateHash, ShortStateKey},
		state::RoomMutexGuard,
		state_compressor::parse_compressed_state_event,
	},
	Dep,
};

pub struct Service {
	pub server_visibility_cache: Mutex<LruCache<(OwnedServerName, ShortStateHash), bool>>,
	pub user_visibility_cache: Mutex<LruCache<(OwnedUserId, ShortStateHash), bool>>,
	services: Services,
	db: Data,
}

struct Services {
	short: Dep<rooms::short::Service>,
	state: Dep<rooms::state::Service>,
	state_compressor: Dep<rooms::state_compressor::Service>,
	state_cache: Dep<rooms::state_cache::Service>,
	timeline: Dep<rooms::timeline::Service>,
}

struct Data {
	shorteventid_shortstatehash: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		let config = &args.server.config;
		let server_visibility_cache_capacity =
			f64::from(config.server_visibility_cache_capacity) * config.cache_capacity_modifier;
		let user_visibility_cache_capacity =
			f64::from(config.user_visibility_cache_capacity) * config.cache_capacity_modifier;

		Ok(Arc::new(Self {
			server_visibility_cache: StdMutex::new(LruCache::new(usize_from_f64(
				server_visibility_cache_capacity,
			)?)),
			user_visibility_cache: StdMutex::new(LruCache::new(usize_from_f64(
				user_visibility_cache_capacity,
			)?)),
			services: Services {
				state_cache: args.depend::<rooms::state_cache::Service>("rooms::state_cache"),
				timeline: args.depend::<rooms::timeline::Service>("rooms::timeline"),
				short: args.depend::<rooms::short::Service>("rooms::short"),
				state: args.depend::<rooms::state::Service>("rooms::state"),
				state_compressor: args
					.depend::<rooms::state_compressor::Service>("rooms::state_compressor"),
			},
			db: Data {
				shorteventid_shortstatehash: args.db["shorteventid_shortstatehash"].clone(),
			},
		}))
	}

	fn memory_usage(&self, out: &mut dyn Write) -> Result {
		use utils::bytes::pretty;

		let (svc_count, svc_bytes) = self.server_visibility_cache.lock()?.iter().fold(
			(0_usize, 0_usize),
			|(count, bytes), (key, _)| {
				(
					count.expected_add(1),
					bytes
						.expected_add(key.0.capacity())
						.expected_add(size_of_val(&key.1)),
				)
			},
		);

		let (uvc_count, uvc_bytes) = self.user_visibility_cache.lock()?.iter().fold(
			(0_usize, 0_usize),
			|(count, bytes), (key, _)| {
				(
					count.expected_add(1),
					bytes
						.expected_add(key.0.capacity())
						.expected_add(size_of_val(&key.1)),
				)
			},
		);

		writeln!(out, "server_visibility_cache: {svc_count} ({})", pretty(svc_bytes))?;
		writeln!(out, "user_visibility_cache: {uvc_count} ({})", pretty(uvc_bytes))?;

		Ok(())
	}

	fn clear_cache(&self) {
		self.server_visibility_cache.lock().expect("locked").clear();
		self.user_visibility_cache.lock().expect("locked").clear();
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	pub async fn state_full(
		&self,
		shortstatehash: ShortStateHash,
	) -> Result<HashMap<(StateEventType, String), PduEvent>> {
		let state = self
			.state_full_pdus(shortstatehash)
			.await?
			.into_iter()
			.filter_map(|pdu| Some(((pdu.kind.to_string().into(), pdu.state_key.clone()?), pdu)))
			.collect();

		Ok(state)
	}

	pub async fn state_full_pdus(&self, shortstatehash: ShortStateHash) -> Result<Vec<PduEvent>> {
		let short_ids = self.state_full_shortids(shortstatehash).await?;

		let full_pdus = self
			.services
			.short
			.multi_get_eventid_from_short(short_ids.into_iter().map(at!(1)).stream())
			.ready_filter_map(Result::ok)
			.broad_filter_map(|event_id: OwnedEventId| async move {
				self.services.timeline.get_pdu(&event_id).await.ok()
			})
			.collect()
			.await;

		Ok(full_pdus)
	}

	/// Builds a StateMap by iterating over all keys that start
	/// with state_hash, this gives the full state for the given state_hash.
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn state_full_ids<Id>(
		&self,
		shortstatehash: ShortStateHash,
	) -> Result<HashMap<ShortStateKey, Id>>
	where
		Id: for<'de> Deserialize<'de> + Send + Sized + ToOwned,
		<Id as ToOwned>::Owned: Borrow<EventId>,
	{
		let short_ids = self.state_full_shortids(shortstatehash).await?;

		let full_ids = self
			.services
			.short
			.multi_get_eventid_from_short(short_ids.iter().map(at!(1)).stream())
			.zip(short_ids.iter().stream().map(at!(0)))
			.ready_filter_map(|(event_id, shortstatekey)| Some((shortstatekey, event_id.ok()?)))
			.collect()
			.boxed()
			.await;

		Ok(full_ids)
	}

	#[inline]
	pub async fn state_full_shortids(
		&self,
		shortstatehash: ShortStateHash,
	) -> Result<Vec<(ShortStateKey, ShortEventId)>> {
		let shortids = self
			.services
			.state_compressor
			.load_shortstatehash_info(shortstatehash)
			.await
			.map_err(|e| err!(Database("Missing state IDs: {e}")))?
			.pop()
			.expect("there is always one layer")
			.full_state
			.iter()
			.copied()
			.map(parse_compressed_state_event)
			.collect();

		Ok(shortids)
	}

	/// Returns a single EventId from `room_id` with key (`event_type`,
	/// `state_key`).
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn state_get_id<Id>(
		&self,
		shortstatehash: ShortStateHash,
		event_type: &StateEventType,
		state_key: &str,
	) -> Result<Id>
	where
		Id: for<'de> Deserialize<'de> + Sized + ToOwned,
		<Id as ToOwned>::Owned: Borrow<EventId>,
	{
		let shortstatekey = self.services.short.get_shortstatekey(event_type, state_key);

		let full_state = self
			.services
			.state_compressor
			.load_shortstatehash_info(shortstatehash)
			.map_ok(|mut vec| vec.pop().expect("there is always one layer").full_state)
			.map_err(|e| err!(Database(error!(?event_type, ?state_key, "Missing state: {e:?}"))));

		let (shortstatekey, full_state) = try_join(shortstatekey, full_state).await?;

		let compressed = full_state
			.iter()
			.find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
			.ok_or(err!(Database("No shortstatekey in compressed state")))?;

		let (_, shorteventid) = parse_compressed_state_event(*compressed);

		self.services
			.short
			.get_eventid_from_short(shorteventid)
			.await
	}

	/// Returns a single PDU from `room_id` with key (`event_type`,
	/// `state_key`).
	#[inline]
	pub async fn state_get(
		&self,
		shortstatehash: ShortStateHash,
		event_type: &StateEventType,
		state_key: &str,
	) -> Result<PduEvent> {
		self.state_get_id(shortstatehash, event_type, state_key)
			.and_then(|event_id: OwnedEventId| async move {
				self.services.timeline.get_pdu(&event_id).await
			})
			.await
	}

	/// Returns a single PDU from `room_id` with key (`event_type`,`state_key`).
	pub async fn state_get_content<T>(
		&self,
		shortstatehash: ShortStateHash,
		event_type: &StateEventType,
		state_key: &str,
	) -> Result<T>
	where
		T: for<'de> Deserialize<'de>,
	{
		self.state_get(shortstatehash, event_type, state_key)
			.await
			.and_then(|event| event.get_content())
	}

	/// Get membership for given user in state
	async fn user_membership(
		&self,
		shortstatehash: ShortStateHash,
		user_id: &UserId,
	) -> MembershipState {
		self.state_get_content(shortstatehash, &StateEventType::RoomMember, user_id.as_str())
			.await
			.map_or(MembershipState::Leave, |c: RoomMemberEventContent| c.membership)
	}

	/// The user was a joined member at this state (potentially in the past)
	#[inline]
	async fn user_was_joined(&self, shortstatehash: ShortStateHash, user_id: &UserId) -> bool {
		self.user_membership(shortstatehash, user_id).await == MembershipState::Join
	}

	/// The user was an invited or joined room member at this state (potentially
	/// in the past)
	#[inline]
	async fn user_was_invited(&self, shortstatehash: ShortStateHash, user_id: &UserId) -> bool {
		let s = self.user_membership(shortstatehash, user_id).await;
		s == MembershipState::Join || s == MembershipState::Invite
	}

	/// Whether a server is allowed to see an event through federation, based on
	/// the room's history_visibility at that event's state.
	#[tracing::instrument(skip_all, level = "trace")]
	pub async fn server_can_see_event(
		&self,
		origin: &ServerName,
		room_id: &RoomId,
		event_id: &EventId,
	) -> bool {
		let Ok(shortstatehash) = self.pdu_shortstatehash(event_id).await else {
			return true;
		};

		if let Some(visibility) = self
			.server_visibility_cache
			.lock()
			.expect("locked")
			.get_mut(&(origin.to_owned(), shortstatehash))
		{
			return *visibility;
		}

		let history_visibility = self
			.state_get_content(shortstatehash, &StateEventType::RoomHistoryVisibility, "")
			.await
			.map_or(HistoryVisibility::Shared, |c: RoomHistoryVisibilityEventContent| {
				c.history_visibility
			});

		let current_server_members = self
			.services
			.state_cache
			.room_members(room_id)
			.ready_filter(|member| member.server_name() == origin);

		let visibility = match history_visibility {
			| HistoryVisibility::WorldReadable | HistoryVisibility::Shared => true,
			| HistoryVisibility::Invited => {
				// Allow if any member on requesting server was AT LEAST invited, else deny
				current_server_members
					.any(|member| self.user_was_invited(shortstatehash, member))
					.await
			},
			| HistoryVisibility::Joined => {
				// Allow if any member on requested server was joined, else deny
				current_server_members
					.any(|member| self.user_was_joined(shortstatehash, member))
					.await
			},
			| _ => {
				error!("Unknown history visibility {history_visibility}");
				false
			},
		};

		self.server_visibility_cache
			.lock()
			.expect("locked")
			.insert((origin.to_owned(), shortstatehash), visibility);

		visibility
	}

	/// Whether a user is allowed to see an event, based on
	/// the room's history_visibility at that event's state.
	#[tracing::instrument(skip_all, level = "trace")]
	pub async fn user_can_see_event(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
		event_id: &EventId,
	) -> bool {
		let Ok(shortstatehash) = self.pdu_shortstatehash(event_id).await else {
			return true;
		};

		if let Some(visibility) = self
			.user_visibility_cache
			.lock()
			.expect("locked")
			.get_mut(&(user_id.to_owned(), shortstatehash))
		{
			return *visibility;
		}

		let currently_member = self.services.state_cache.is_joined(user_id, room_id).await;

		let history_visibility = self
			.state_get_content(shortstatehash, &StateEventType::RoomHistoryVisibility, "")
			.await
			.map_or(HistoryVisibility::Shared, |c: RoomHistoryVisibilityEventContent| {
				c.history_visibility
			});

		let visibility = match history_visibility {
			| HistoryVisibility::WorldReadable => true,
			| HistoryVisibility::Shared => currently_member,
			| HistoryVisibility::Invited => {
				// Allow if any member on requesting server was AT LEAST invited, else deny
				self.user_was_invited(shortstatehash, user_id).await
			},
			| HistoryVisibility::Joined => {
				// Allow if any member on requested server was joined, else deny
				self.user_was_joined(shortstatehash, user_id).await
			},
			| _ => {
				error!("Unknown history visibility {history_visibility}");
				false
			},
		};

		self.user_visibility_cache
			.lock()
			.expect("locked")
			.insert((user_id.to_owned(), shortstatehash), visibility);

		visibility
	}

	/// Whether a user is allowed to see an event, based on
	/// the room's history_visibility at that event's state.
	#[tracing::instrument(skip_all, level = "trace")]
	pub async fn user_can_see_state_events(&self, user_id: &UserId, room_id: &RoomId) -> bool {
		if self.services.state_cache.is_joined(user_id, room_id).await {
			return true;
		}

		let history_visibility = self
			.room_state_get_content(room_id, &StateEventType::RoomHistoryVisibility, "")
			.await
			.map_or(HistoryVisibility::Shared, |c: RoomHistoryVisibilityEventContent| {
				c.history_visibility
			});

		match history_visibility {
			| HistoryVisibility::Invited =>
				self.services.state_cache.is_invited(user_id, room_id).await,
			| HistoryVisibility::WorldReadable => true,
			| _ => false,
		}
	}

	/// Returns the state hash for this pdu.
	pub async fn pdu_shortstatehash(&self, event_id: &EventId) -> Result<ShortStateHash> {
		const BUFSIZE: usize = size_of::<ShortEventId>();

		self.services
			.short
			.get_shorteventid(event_id)
			.and_then(|shorteventid| {
				self.db
					.shorteventid_shortstatehash
					.aqry::<BUFSIZE, _>(&shorteventid)
			})
			.await
			.deserialized()
	}

	/// Returns the full room state.
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn room_state_full(
		&self,
		room_id: &RoomId,
	) -> Result<HashMap<(StateEventType, String), PduEvent>> {
		self.services
			.state
			.get_room_shortstatehash(room_id)
			.and_then(|shortstatehash| self.state_full(shortstatehash))
			.map_err(|e| err!(Database("Missing state for {room_id:?}: {e:?}")))
			.await
	}

	/// Returns the full room state pdus
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn room_state_full_pdus(&self, room_id: &RoomId) -> Result<Vec<PduEvent>> {
		self.services
			.state
			.get_room_shortstatehash(room_id)
			.and_then(|shortstatehash| self.state_full_pdus(shortstatehash))
			.map_err(|e| err!(Database("Missing state pdus for {room_id:?}: {e:?}")))
			.await
	}

	/// Returns a single EventId from `room_id` with key (`event_type`,
	/// `state_key`).
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn room_state_get_id<Id>(
		&self,
		room_id: &RoomId,
		event_type: &StateEventType,
		state_key: &str,
	) -> Result<Id>
	where
		Id: for<'de> Deserialize<'de> + Sized + ToOwned,
		<Id as ToOwned>::Owned: Borrow<EventId>,
	{
		self.services
			.state
			.get_room_shortstatehash(room_id)
			.and_then(|shortstatehash| self.state_get_id(shortstatehash, event_type, state_key))
			.await
	}

	/// Returns a single PDU from `room_id` with key (`event_type`,
	/// `state_key`).
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn room_state_get(
		&self,
		room_id: &RoomId,
		event_type: &StateEventType,
		state_key: &str,
	) -> Result<PduEvent> {
		self.services
			.state
			.get_room_shortstatehash(room_id)
			.and_then(|shortstatehash| self.state_get(shortstatehash, event_type, state_key))
			.await
	}

	/// Returns a single PDU from `room_id` with key (`event_type`,`state_key`).
	pub async fn room_state_get_content<T>(
		&self,
		room_id: &RoomId,
		event_type: &StateEventType,
		state_key: &str,
	) -> Result<T>
	where
		T: for<'de> Deserialize<'de>,
	{
		self.room_state_get(room_id, event_type, state_key)
			.await
			.and_then(|event| event.get_content())
	}

	pub async fn get_name(&self, room_id: &RoomId) -> Result<String> {
		self.room_state_get_content(room_id, &StateEventType::RoomName, "")
			.await
			.map(|c: RoomNameEventContent| c.name)
	}

	pub async fn get_avatar(&self, room_id: &RoomId) -> JsOption<RoomAvatarEventContent> {
		let content = self
			.room_state_get_content(room_id, &StateEventType::RoomAvatar, "")
			.await
			.ok();

		JsOption::from_option(content)
	}

	pub async fn get_member(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<RoomMemberEventContent> {
		self.room_state_get_content(room_id, &StateEventType::RoomMember, user_id.as_str())
			.await
	}

	pub async fn user_can_invite(
		&self,
		room_id: &RoomId,
		sender: &UserId,
		target_user: &UserId,
		state_lock: &RoomMutexGuard,
	) -> bool {
		self.services
			.timeline
			.create_hash_and_sign_event(
				PduBuilder::state(
					target_user.into(),
					&RoomMemberEventContent::new(MembershipState::Invite),
				),
				sender,
				room_id,
				state_lock,
			)
			.await
			.is_ok()
	}

	/// Checks if guests are able to view room content without joining
	pub async fn is_world_readable(&self, room_id: &RoomId) -> bool {
		self.room_state_get_content(room_id, &StateEventType::RoomHistoryVisibility, "")
			.await
			.map(|c: RoomHistoryVisibilityEventContent| {
				c.history_visibility == HistoryVisibility::WorldReadable
			})
			.unwrap_or(false)
	}

	/// Checks if guests are able to join a given room
	pub async fn guest_can_join(&self, room_id: &RoomId) -> bool {
		self.room_state_get_content(room_id, &StateEventType::RoomGuestAccess, "")
			.await
			.map(|c: RoomGuestAccessEventContent| c.guest_access == GuestAccess::CanJoin)
			.unwrap_or(false)
	}

	/// Gets the primary alias from canonical alias event
	pub async fn get_canonical_alias(&self, room_id: &RoomId) -> Result<OwnedRoomAliasId> {
		self.room_state_get_content(room_id, &StateEventType::RoomCanonicalAlias, "")
			.await
			.and_then(|c: RoomCanonicalAliasEventContent| {
				c.alias
					.ok_or_else(|| err!(Request(NotFound("No alias found in event content."))))
			})
	}

	/// Gets the room topic
	pub async fn get_room_topic(&self, room_id: &RoomId) -> Result<String> {
		self.room_state_get_content(room_id, &StateEventType::RoomTopic, "")
			.await
			.map(|c: RoomTopicEventContent| c.topic)
	}

	/// Checks if a given user can redact a given event
	///
	/// If federation is true, it allows redaction events from any user of the
	/// same server as the original event sender
	pub async fn user_can_redact(
		&self,
		redacts: &EventId,
		sender: &UserId,
		room_id: &RoomId,
		federation: bool,
	) -> Result<bool> {
		let redacting_event = self.services.timeline.get_pdu(redacts).await;

		if redacting_event
			.as_ref()
			.is_ok_and(|pdu| pdu.kind == TimelineEventType::RoomCreate)
		{
			return Err!(Request(Forbidden("Redacting m.room.create is not safe, forbidding.")));
		}

		if redacting_event
			.as_ref()
			.is_ok_and(|pdu| pdu.kind == TimelineEventType::RoomServerAcl)
		{
			return Err!(Request(Forbidden(
				"Redacting m.room.server_acl will result in the room being inaccessible for \
				 everyone (empty allow key), forbidding."
			)));
		}

		if let Ok(pl_event_content) = self
			.room_state_get_content::<RoomPowerLevelsEventContent>(
				room_id,
				&StateEventType::RoomPowerLevels,
				"",
			)
			.await
		{
			let pl_event: RoomPowerLevels = pl_event_content.into();
			Ok(pl_event.user_can_redact_event_of_other(sender)
				|| pl_event.user_can_redact_own_event(sender)
					&& if let Ok(redacting_event) = redacting_event {
						if federation {
							redacting_event.sender.server_name() == sender.server_name()
						} else {
							redacting_event.sender == sender
						}
					} else {
						false
					})
		} else {
			// Falling back on m.room.create to judge power level
			if let Ok(room_create) = self
				.room_state_get(room_id, &StateEventType::RoomCreate, "")
				.await
			{
				Ok(room_create.sender == sender
					|| redacting_event
						.as_ref()
						.is_ok_and(|redacting_event| redacting_event.sender == sender))
			} else {
				Err(Error::bad_database(
					"No m.room.power_levels or m.room.create events in database for room",
				))
			}
		}
	}

	/// Returns the join rule (`SpaceRoomJoinRule`) for a given room
	pub async fn get_join_rule(
		&self,
		room_id: &RoomId,
	) -> Result<(SpaceRoomJoinRule, Vec<OwnedRoomId>)> {
		self.room_state_get_content(room_id, &StateEventType::RoomJoinRules, "")
			.await
			.map(|c: RoomJoinRulesEventContent| {
				(c.join_rule.clone().into(), self.allowed_room_ids(c.join_rule))
			})
			.or_else(|_| Ok((SpaceRoomJoinRule::Invite, vec![])))
	}

	/// Returns an empty vec if not a restricted room
	pub fn allowed_room_ids(&self, join_rule: JoinRule) -> Vec<OwnedRoomId> {
		let mut room_ids = Vec::with_capacity(1);
		if let JoinRule::Restricted(r) | JoinRule::KnockRestricted(r) = join_rule {
			for rule in r.allow {
				if let AllowRule::RoomMembership(RoomMembership { room_id: membership }) = rule {
					room_ids.push(membership.clone());
				}
			}
		}
		room_ids
	}

	pub async fn get_room_type(&self, room_id: &RoomId) -> Result<RoomType> {
		self.room_state_get_content(room_id, &StateEventType::RoomCreate, "")
			.await
			.and_then(|content: RoomCreateEventContent| {
				content
					.room_type
					.ok_or_else(|| err!(Request(NotFound("No type found in event content"))))
			})
	}

	/// Gets the room's encryption algorithm if `m.room.encryption` state event
	/// is found
	pub async fn get_room_encryption(
		&self,
		room_id: &RoomId,
	) -> Result<EventEncryptionAlgorithm> {
		self.room_state_get_content(room_id, &StateEventType::RoomEncryption, "")
			.await
			.map(|content: RoomEncryptionEventContent| content.algorithm)
	}

	pub async fn is_encrypted_room(&self, room_id: &RoomId) -> bool {
		self.room_state_get(room_id, &StateEventType::RoomEncryption, "")
			.await
			.is_ok()
	}
}
