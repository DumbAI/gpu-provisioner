-- gpuprov's state in Postgres: the `vms` projection and the machine journal.
-- `{s}` is the (quoted) schema; gpuprov never touches anything outside it.
-- Times are the server's (`now()`), so leases compare against one clock.

CREATE TABLE {s}.vms (
  name            text PRIMARY KEY,
  state           text NOT NULL,
  provider        text,
  region          text,
  instance_type   text,
  zone            text,
  instance_id     text,
  public_ip       text,
  private_ip      text,
  ssh_user        text,
  ssh_port        integer,
  ssh_key_path    text,
  ssh_config_path text,
  spec_json       text NOT NULL,
  created_at      timestamptz NOT NULL DEFAULT now(),
  updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE {s}.machines (
  key        text PRIMARY KEY,
  kind       text NOT NULL,
  state_json text NOT NULL,
  label      text NOT NULL,
  terminal   boolean NOT NULL,
  -- The lease: `LeaseOwner` (host:pid:token) and its last heartbeat.
  owner      text,
  heartbeat  timestamptz,
  updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE {s}.machine_events (
  seq    bigserial PRIMARY KEY,
  key    text NOT NULL,
  kind   text NOT NULL,
  ts     timestamptz NOT NULL DEFAULT now(),
  label  text NOT NULL,
  detail text
);

CREATE INDEX machine_events_key_seq ON {s}.machine_events (key, seq);
