# Load-test scenarios. Each scenario target starts the stack it needs, runs
# one measurement with `tgin-bench run` and leaves the stack up until the next
# `make clean`:
#
#   make image                      # build the engine image (once per change)
#   make webhook-tgin-3 RPS=5000 DURATION=30
#
# ENGINE selects the engine under test:
#   v1  tgin 1.x: image tgin-bench:v1 built from this tree, YAML configs in
#       cfgs/v1/, data plane on 8080, admin plane on 9090 (/readyz).
#   v0  tgin 0.2: image tgin-bench:v0 built from V0_REF, RON configs in cfgs/,
#       management API on 3000 (webhook) or 3030 (long poll).
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
# The last 0.2 commit, which `make image ENGINE=v0` builds.
V0_REF ?= 1af6aaa
TGIN_IMAGE ?= tgin-bench:$(ENGINE)

TGIN_IP := 10.5.0.2

ifeq ($(ENGINE),v1)
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
else ifeq ($(ENGINE),v0)
TGIN_CONFIG_TARGET := /etc/tgin/config.ron
# The RON configs serve webhooks on 3000 and long-poll routes on 3030; the
# scenario targets below pick the port.
TGIN_PROBE_PORT = $(V0_PORT)
READY_URL = http://localhost:$(V0_PORT)/api/routes
WEBHOOK_URL := http://$(TGIN_IP):3000/webhook
CFG_WEBHOOK_1 := ./cfgs/webhook-one.ron
# Two routes; the third is added through the management API.
CFG_WEBHOOK_3 := ./cfgs/webhook.ron
CFG_LONGPOLL_1 := ./cfgs/longpull-one.ron
# Two routes; the third is added through the management API.
CFG_LONGPOLL_3 := ./cfgs/longpull.ron
PULL_URL_1 := http://$(TGIN_IP):3030/bot1/getUpdates
PULL_URL_2 := http://$(TGIN_IP):3030/bot2/getUpdates
PULL_URL_3 := http://$(TGIN_IP):3030/bot3/getUpdates
else
$(error ENGINE must be v0 or v1, got '$(ENGINE)')
endif

webhook-%: V0_PORT := 3000
longpoll-%: V0_PORT := 3030

# Read by docker-compose.yml.
export TGIN_IMAGE TGIN_CONFIG_FILE TGIN_CONFIG_TARGET TGIN_PROBE_PORT BOT_SLEEP_MS \
	PULL_URL_1 PULL_URL_2 PULL_URL_3

# Must match `auth_token` in cfgs/*.ron (v0) and `admin.token` in cfgs/v1/*.yaml.
AUTH_HEADER := Authorization: Bearer bench
# The third downstream of the v0 "-3" scenarios, as 0.2 management API routes.
V0_WEBHOOK_ROUTE_3 := {"type": "Webhook", "url": "http://10.5.0.13:8080/webhook"}
V0_LONGPOLL_ROUTE_3 := {"type": "Longpull", "path": "/bot3/getUpdates"}

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

# v0 only: add a route through the 0.2 management API.
define ADD_V0_ROUTE
@curl --fail --silent --show-error --connect-timeout 2 --max-time 10 --request POST "http://localhost:$(V0_PORT)/api/route" --data '$(1)' --header "Content-Type: application/json" --header "$(AUTH_HEADER)" >/dev/null
endef

clean:
	docker compose down --remove-orphans

build: clean
	docker compose build bench webhook_bot1

image:
ifeq ($(ENGINE),v1)
	docker build -t "$(TGIN_IMAGE)" ../..
else
	git -C ../.. archive --format=tar "$(V0_REF)" | docker build -t "$(TGIN_IMAGE)" -
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
ifeq ($(ENGINE),v0)
	$(call ADD_V0_ROUTE,$(V0_WEBHOOK_ROUTE_3))
endif
	$(call RUN_BENCH,--target $(WEBHOOK_URL))

longpoll-direct-1: clean
	$(call COMPOSE_UP_WAIT,bench longpull_bot_direct)
	$(call RUN_BENCH,--mode longpoll)

longpoll-tgin-1: TGIN_CONFIG_FILE := $(CFG_LONGPOLL_1)
longpoll-tgin-1: clean
	$(call COMPOSE_UP_WAIT,bench tgin longpull_bot1)
	$(call WAIT_HTTP,$(READY_URL))
	$(call RUN_BENCH,--mode longpoll)

longpoll-tgin-3: TGIN_CONFIG_FILE := $(CFG_LONGPOLL_3)
longpoll-tgin-3: clean
	$(call COMPOSE_UP_WAIT,bench tgin longpull_bot1 longpull_bot2)
	$(call WAIT_HTTP,$(READY_URL))
ifeq ($(ENGINE),v0)
	$(call ADD_V0_ROUTE,$(V0_LONGPOLL_ROUTE_3))
endif
	$(call COMPOSE_UP_WAIT,longpull_bot3)
	$(call RUN_BENCH,--mode longpoll)
