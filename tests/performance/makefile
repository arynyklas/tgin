# Load-test scenarios. Each scenario target starts the stack it needs, runs
# one measurement with `tgin-bench run` and leaves the stack up until the next
# `make clean`:
#
#   make image                      # build the engine image (once per change)
#   make webhook-tgin-3 RPS=5000 DURATION=30
#
# ENGINE selects the engine under test:
#   v1        tgin 1.x: image built from this tree, YAML configs in cfgs/v1/,
#             data plane on 8080, admin plane on 9090 (/readyz).
#   upstream  the original tgin (github.com/chesnokpeter/tgin) at
#             UPSTREAM_REF, built with its own Dockerfile; the original
#             engine's configs in cfgs/upstream/; management API (no auth)
#             on 3000 (webhook scenarios) or 3030 (long-poll scenarios).
#             Readiness is `GET /api/routes`; downstreams beyond the configs
#             are added with `POST /api/route`. Its configured long-poll
#             routes accept only form-encoded polls, so every long-poll route
#             is added through the management API, whose fallback handler
#             also accepts JSON.
# V1_IMAGE and UPSTREAM_IMAGE name the image of each engine.
#
# BOT_IMPL selects the downstream bots:
#   rust  `tgin-bench bot` (default): answers far faster than any target rate,
#         so a scenario measures the engine, not the bots.
#   py    aiogram 3 bots (bot/); one process answers about 2 000 updates/s.

BOT_IMPL ?= rust
ifeq ($(BOT_IMPL),rust)
export COMPOSE_FILE := docker-compose.yml:docker-compose.bot-rust.yml
export COMPOSE_PATH_SEPARATOR := :
else ifneq ($(BOT_IMPL),py)
$(error BOT_IMPL must be rust or py, got '$(BOT_IMPL)')
endif

ENGINE ?= v1
RPS ?= 1000
DURATION ?= 30
BENCH_FORMAT ?= text
# Append the run's output to this file instead of printing it.
BENCH_OUTPUT ?=
# Per-update work each bot simulates before replying, milliseconds.
BOT_SLEEP_MS ?= 0
# How long a run waits for outstanding replies after the send phase, seconds.
COMPLETION_TIMEOUT ?= 30
COMPOSE_WAIT_TIMEOUT ?= 90
# The last upstream commit this project contains, which
# `make image ENGINE=upstream` builds.
UPSTREAM_REF ?= 4f0971a
V1_IMAGE ?= tgin-bench:v1
UPSTREAM_IMAGE ?= tgin-bench:upstream

TGIN_IP := 10.5.0.2

ifeq ($(ENGINE),v1)
TGIN_IMAGE := $(V1_IMAGE)
TGIN_CONFIG_TARGET := /etc/tgin/tgin.yaml
TGIN_PROBE_PORT := 9090
READY_URL := http://localhost:9090/readyz
WEBHOOK_URL := http://$(TGIN_IP):8080/webhook
CFG_WEBHOOK_1 := ./cfgs/v1/webhook-1.yaml
CFG_WEBHOOK_3 := ./cfgs/v1/webhook-3.yaml
CFG_LONGPOLL_1 := ./cfgs/v1/longpoll-1.yaml
CFG_LONGPOLL_3 := ./cfgs/v1/longpoll-3.yaml
# Every bot instance polls the one pull consumer; tgin tells instances apart
# by their address. `/bot…` paths are reserved in v1, so the bots use the
# consumer's own path instead of an alias.
PULL_URL_1 := http://$(TGIN_IP):8080/c/bots/getUpdates
PULL_URL_2 := $(PULL_URL_1)
PULL_URL_3 := $(PULL_URL_1)
else ifeq ($(ENGINE),upstream)
TGIN_IMAGE := $(UPSTREAM_IMAGE)
# The path the upstream image's CMD (`tgin -f /etc/tgin/config.ron`) reads.
TGIN_CONFIG_TARGET := /etc/tgin/config.ron
# The configs serve webhooks on 3000 and long-poll routes on 3030; the
# scenario targets below pick the port.
TGIN_PROBE_PORT = $(UPSTREAM_PORT)
READY_URL = http://localhost:$(UPSTREAM_PORT)/api/routes
WEBHOOK_URL := http://$(TGIN_IP):3000/webhook
CFG_WEBHOOK_1 := ./cfgs/upstream/webhook-one.ron
# Two routes; the third is added through the management API.
CFG_WEBHOOK_3 := ./cfgs/upstream/webhook.ron
# No routes: all of them are added through the management API.
CFG_LONGPOLL_1 := ./cfgs/upstream/longpull.ron
CFG_LONGPOLL_3 := ./cfgs/upstream/longpull.ron
PULL_URL_1 := http://$(TGIN_IP):3030/bot1/getUpdates
PULL_URL_2 := http://$(TGIN_IP):3030/bot2/getUpdates
PULL_URL_3 := http://$(TGIN_IP):3030/bot3/getUpdates
else
$(error ENGINE must be v1 or upstream, got '$(ENGINE)')
endif

webhook-%: UPSTREAM_PORT := 3000
longpoll-%: UPSTREAM_PORT := 3030

# Read by docker-compose.yml.
export TGIN_IMAGE TGIN_CONFIG_FILE TGIN_CONFIG_TARGET TGIN_PROBE_PORT BOT_SLEEP_MS \
	PULL_URL_1 PULL_URL_2 PULL_URL_3

# Must match `admin.token` in cfgs/v1/*.yaml; upstream's management API has
# no auth and ignores it.
AUTH_HEADER := Authorization: Bearer bench
# Downstreams the upstream scenarios add through the management API.
UPSTREAM_WEBHOOK_ROUTE_3 := {"type": "Webhook", "url": "http://10.5.0.13:8080/webhook"}
UPSTREAM_LONGPOLL_ROUTE_1 := {"type": "Longpull", "path": "/bot1/getUpdates"}
UPSTREAM_LONGPOLL_ROUTE_2 := {"type": "Longpull", "path": "/bot2/getUpdates"}
UPSTREAM_LONGPOLL_ROUTE_3 := {"type": "Longpull", "path": "/bot3/getUpdates"}

SCENARIOS := webhook-direct-1 webhook-tgin-1 webhook-tgin-3 \
	longpoll-direct-1 longpoll-tgin-1 longpoll-tgin-3

.PHONY: clean build image $(SCENARIOS)

define RUN_BENCH
docker compose exec -T bench tgin-bench run $(1) \
    --rps "$(RPS)" \
    --duration "$(DURATION)" \
    --scenario "$@" \
    --engine "$(ENGINE)" \
    --completion-timeout "$(COMPLETION_TIMEOUT)" \
    --format "$(BENCH_FORMAT)" $(if $(BENCH_OUTPUT),>> "$(BENCH_OUTPUT)")
endef

define COMPOSE_UP_WAIT
docker compose up -d --wait --wait-timeout $(COMPOSE_WAIT_TIMEOUT) $(1)
endef

define WAIT_HTTP
@echo "Waiting for $(1) ..."
@deadline=$$(($$(date +%s) + $(COMPOSE_WAIT_TIMEOUT))); \
while [ $$(date +%s) -lt $$deadline ]; do \
    if curl --fail --silent --connect-timeout 1 --max-time 2 --header "$(AUTH_HEADER)" "$(1)" >/dev/null; then exit 0; fi; \
    sleep 1; \
done; \
echo "Timed out waiting for $(1)" >&2; \
curl --fail --silent --show-error --connect-timeout 1 --max-time 2 --header "$(AUTH_HEADER)" "$(1)" >/dev/null; \
exit 1
endef

# Upstream only: add a route through the management API. Upstream queues the
# change and answers at once; it applies its management messages in order,
# so the readiness GET after the last add returns once every add is applied.
define ADD_ROUTE
@curl --fail --silent --show-error --connect-timeout 2 --max-time 10 --request POST "http://localhost:$(UPSTREAM_PORT)/api/route" --data '$(1)' --header "Content-Type: application/json" >/dev/null
endef

clean:
	docker compose down --remove-orphans

build: clean
	docker compose build bench webhook_bot1

image:
ifeq ($(ENGINE),v1)
	docker build -t "$(TGIN_IMAGE)" ../..
else
	git -C ../.. archive --format=tar "$(UPSTREAM_REF)" | docker build -t "$(TGIN_IMAGE)" -
endif

webhook-direct-1: clean
	$(call COMPOSE_UP_WAIT,bench webhook_bot1)
	$(call RUN_BENCH,--target http://10.5.0.11:8080/webhook)

webhook-tgin-1: TGIN_CONFIG_FILE := $(CFG_WEBHOOK_1)
webhook-tgin-1: clean
	$(call COMPOSE_UP_WAIT,bench webhook_bot1 tgin)
	$(call WAIT_HTTP,$(READY_URL))
	$(call RUN_BENCH,--target $(WEBHOOK_URL))

webhook-tgin-3: TGIN_CONFIG_FILE := $(CFG_WEBHOOK_3)
webhook-tgin-3: clean
	$(call COMPOSE_UP_WAIT,bench webhook_bot1 webhook_bot2 webhook_bot3 tgin)
	$(call WAIT_HTTP,$(READY_URL))
ifeq ($(ENGINE),upstream)
	$(call ADD_ROUTE,$(UPSTREAM_WEBHOOK_ROUTE_3))
	$(call WAIT_HTTP,$(READY_URL))
endif
	$(call RUN_BENCH,--target $(WEBHOOK_URL))

longpoll-direct-1: clean
	$(call COMPOSE_UP_WAIT,bench longpull_bot_direct)
	$(call RUN_BENCH,--mode longpoll)

longpoll-tgin-1: TGIN_CONFIG_FILE := $(CFG_LONGPOLL_1)
longpoll-tgin-1: clean
ifeq ($(ENGINE),upstream)
	$(call COMPOSE_UP_WAIT,bench tgin)
	$(call WAIT_HTTP,$(READY_URL))
	$(call ADD_ROUTE,$(UPSTREAM_LONGPOLL_ROUTE_1))
	$(call WAIT_HTTP,$(READY_URL))
	$(call COMPOSE_UP_WAIT,longpull_bot1)
else
	$(call COMPOSE_UP_WAIT,bench tgin longpull_bot1)
	$(call WAIT_HTTP,$(READY_URL))
endif
	$(call RUN_BENCH,--mode longpoll)

longpoll-tgin-3: TGIN_CONFIG_FILE := $(CFG_LONGPOLL_3)
longpoll-tgin-3: clean
ifeq ($(ENGINE),upstream)
	$(call COMPOSE_UP_WAIT,bench tgin)
	$(call WAIT_HTTP,$(READY_URL))
	$(call ADD_ROUTE,$(UPSTREAM_LONGPOLL_ROUTE_1))
	$(call ADD_ROUTE,$(UPSTREAM_LONGPOLL_ROUTE_2))
	$(call ADD_ROUTE,$(UPSTREAM_LONGPOLL_ROUTE_3))
	$(call WAIT_HTTP,$(READY_URL))
	$(call COMPOSE_UP_WAIT,longpull_bot1 longpull_bot2 longpull_bot3)
else
	$(call COMPOSE_UP_WAIT,bench tgin longpull_bot1 longpull_bot2)
	$(call WAIT_HTTP,$(READY_URL))
	$(call COMPOSE_UP_WAIT,longpull_bot3)
endif
	$(call RUN_BENCH,--mode longpoll)
