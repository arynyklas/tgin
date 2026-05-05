RPS ?= 100
DURATION ?= 30
BENCH_PORT := 8090
BENCH_FORMAT ?= text
RUN_ID ?= manual
GIT_SHA ?= unknown
SCENARIO_FAMILY ?= performance
TRANSPORT ?= unknown
ROUTE_PATH ?= unknown
SCENARIO ?= unknown
BOT_COUNT ?= 1
DRAIN_TIMEOUT ?= 2
BENCH_OUTPUT ?=
COMPOSE_WAIT_TIMEOUT ?= 90
TGIN_WEBHOOK_API := http://localhost:3000/api/routes
TGIN_LONGPOLL_API := http://localhost:3030/api/routes

.PHONY: clean build \
	webhook-direct-1 webhook-tgin-1 webhook-tgin-scale-2 webhook-tgin-scale-3 webhook-tgin-scale-5 webhook-tgin-scale-10 \
	longpoll-direct-1 longpoll-tgin-1 longpoll-tgin-scale-2 longpoll-tgin-scale-3 longpoll-tgin-scale-5 longpoll-tgin-scale-10

define RUN_BENCH
@if [ -n "$(BENCH_OUTPUT)" ]; then \
    docker compose exec -T bench tgin-bench $(1) \
        --format "$(BENCH_FORMAT)" \
        --run-id "$(RUN_ID)" \
        --git-sha "$(GIT_SHA)" \
        --scenario-family "$(SCENARIO_FAMILY)" \
        --transport "$(TRANSPORT)" \
        --route-path "$(ROUTE_PATH)" \
        --scenario "$(SCENARIO)" \
        --bot-count "$(BOT_COUNT)" \
        --drain-timeout "$(DRAIN_TIMEOUT)" >> "$(BENCH_OUTPUT)"; \
else \
    docker compose exec -T bench tgin-bench $(1) \
        --format "$(BENCH_FORMAT)" \
        --run-id "$(RUN_ID)" \
        --git-sha "$(GIT_SHA)" \
        --scenario-family "$(SCENARIO_FAMILY)" \
        --transport "$(TRANSPORT)" \
        --route-path "$(ROUTE_PATH)" \
        --scenario "$(SCENARIO)" \
        --bot-count "$(BOT_COUNT)" \
        --drain-timeout "$(DRAIN_TIMEOUT)"; \
fi
endef

define COMPOSE_UP_WAIT
docker compose up -d --wait --wait-timeout $(COMPOSE_WAIT_TIMEOUT) $(1)
endef

define WAIT_HTTP
@echo "Waiting for $(1) ..."
@deadline=$$(($$(date +%s) + $(COMPOSE_WAIT_TIMEOUT))); \
while [ $$(date +%s) -lt $$deadline ]; do \
    remaining=$$((deadline - $$(date +%s))); \
    if [ $$remaining -le 0 ]; then break; fi; \
    curl_timeout=$$remaining; \
    if [ $$curl_timeout -gt 2 ]; then curl_timeout=2; fi; \
    if curl --fail --silent --connect-timeout 1 --max-time $$curl_timeout "$(1)" >/dev/null; then exit 0; fi; \
    sleep 1; \
done; \
echo "Timed out waiting for $(1)" >&2; \
curl --fail --silent --show-error --connect-timeout 1 --max-time 2 "$(1)" >/dev/null; \
exit 1
endef

define ADD_WEBHOOK_ROUTE
	@curl --fail --silent --show-error --connect-timeout 2 --max-time 10 --request POST http://localhost:$(1)/api/route --data '{ "type": "Webhook", "url": "$(2)"}' --header "Content-Type: application/json" >/dev/null
endef

define ADD_LONGPULL_ROUTE
	@curl --fail --silent --show-error --connect-timeout 2 --max-time 10 --request POST http://localhost:$(1)/api/route --data '{ "type": "Longpull", "path": "$(2)"}' --header "Content-Type: application/json" >/dev/null
endef

clean:
	docker compose down

build: clean
	docker compose build

webhook-direct-1: SCENARIO := webhook-direct-1
webhook-direct-1: SCENARIO_FAMILY := overhead
webhook-direct-1: TRANSPORT := webhook
webhook-direct-1: ROUTE_PATH := /webhook
webhook-direct-1: BOT_COUNT := 1
webhook-direct-1: clean
	$(call COMPOSE_UP_WAIT,webhook_bot1 bench)
	$(call RUN_BENCH,--target http://10.5.0.11:8080/webhook --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))

webhook-tgin-1: SCENARIO := webhook-tgin-1
webhook-tgin-1: SCENARIO_FAMILY := overhead
webhook-tgin-1: TRANSPORT := webhook
webhook-tgin-1: ROUTE_PATH := /webhook
webhook-tgin-1: BOT_COUNT := 1
webhook-tgin-1: clean
	$(call COMPOSE_UP_WAIT,tgin_webhook_one webhook_bot1 bench)
	$(call WAIT_HTTP,$(TGIN_WEBHOOK_API))
	$(call RUN_BENCH,--target http://10.5.0.2:3000/webhook --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))

webhook-tgin-scale-2: SCENARIO := webhook-tgin-scale-2
webhook-tgin-scale-2: SCENARIO_FAMILY := scale
webhook-tgin-scale-2: TRANSPORT := webhook
webhook-tgin-scale-2: ROUTE_PATH := /webhook
webhook-tgin-scale-2: BOT_COUNT := 2
webhook-tgin-scale-2: clean
	$(call COMPOSE_UP_WAIT,tgin_webhook webhook_bot1 webhook_bot2 bench)
	$(call WAIT_HTTP,$(TGIN_WEBHOOK_API))
	$(call RUN_BENCH,--target http://10.5.0.2:3000/webhook --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))

webhook-tgin-scale-3: SCENARIO := webhook-tgin-scale-3
webhook-tgin-scale-3: SCENARIO_FAMILY := scale
webhook-tgin-scale-3: TRANSPORT := webhook
webhook-tgin-scale-3: ROUTE_PATH := /webhook
webhook-tgin-scale-3: BOT_COUNT := 3
webhook-tgin-scale-3: clean
	$(call COMPOSE_UP_WAIT,tgin_webhook webhook_bot1 webhook_bot2 bench webhook_bot3)
	$(call WAIT_HTTP,$(TGIN_WEBHOOK_API))
	$(call ADD_WEBHOOK_ROUTE,3000,http://10.5.0.13:8080/webhook)
	$(call RUN_BENCH,--target http://10.5.0.2:3000/webhook --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))

webhook-tgin-scale-5: SCENARIO := webhook-tgin-scale-5
webhook-tgin-scale-5: SCENARIO_FAMILY := scale
webhook-tgin-scale-5: TRANSPORT := webhook
webhook-tgin-scale-5: ROUTE_PATH := /webhook
webhook-tgin-scale-5: BOT_COUNT := 5
webhook-tgin-scale-5: clean
	$(call COMPOSE_UP_WAIT,tgin_webhook webhook_bot1 webhook_bot2 bench webhook_bot3 webhook_bot4 webhook_bot5)
	$(call WAIT_HTTP,$(TGIN_WEBHOOK_API))
	$(call ADD_WEBHOOK_ROUTE,3000,http://10.5.0.13:8080/webhook)
	$(call ADD_WEBHOOK_ROUTE,3000,http://10.5.0.14:8080/webhook)
	$(call ADD_WEBHOOK_ROUTE,3000,http://10.5.0.15:8080/webhook)
	$(call RUN_BENCH,--target http://10.5.0.2:3000/webhook --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))

webhook-tgin-scale-10: SCENARIO := webhook-tgin-scale-10
webhook-tgin-scale-10: SCENARIO_FAMILY := scale
webhook-tgin-scale-10: TRANSPORT := webhook
webhook-tgin-scale-10: ROUTE_PATH := /webhook
webhook-tgin-scale-10: BOT_COUNT := 10
webhook-tgin-scale-10: clean
	$(call COMPOSE_UP_WAIT,tgin_webhook webhook_bot1 webhook_bot2 bench webhook_bot3 webhook_bot4 webhook_bot5 webhook_bot6 webhook_bot7 webhook_bot8 webhook_bot9 webhook_bot10)
	$(call WAIT_HTTP,$(TGIN_WEBHOOK_API))
	$(call ADD_WEBHOOK_ROUTE,3000,http://10.5.0.13:8080/webhook)
	$(call ADD_WEBHOOK_ROUTE,3000,http://10.5.0.14:8080/webhook)
	$(call ADD_WEBHOOK_ROUTE,3000,http://10.5.0.15:8080/webhook)
	$(call ADD_WEBHOOK_ROUTE,3000,http://10.5.0.16:8080/webhook)
	$(call ADD_WEBHOOK_ROUTE,3000,http://10.5.0.17:8080/webhook)
	$(call ADD_WEBHOOK_ROUTE,3000,http://10.5.0.18:8080/webhook)
	$(call ADD_WEBHOOK_ROUTE,3000,http://10.5.0.19:8080/webhook)
	$(call ADD_WEBHOOK_ROUTE,3000,http://10.5.0.199:8080/webhook)
	$(call RUN_BENCH,--target http://10.5.0.2:3000/webhook --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))

longpoll-direct-1: SCENARIO := longpoll-direct-1
longpoll-direct-1: SCENARIO_FAMILY := overhead
longpoll-direct-1: TRANSPORT := longpoll
longpoll-direct-1: ROUTE_PATH := /bot123:test/getUpdates
longpoll-direct-1: BOT_COUNT := 1
longpoll-direct-1: clean
	$(call COMPOSE_UP_WAIT,longpull_bot_direct bench)
	$(call RUN_BENCH,--mode longpoll --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))

longpoll-tgin-1: SCENARIO := longpoll-tgin-1
longpoll-tgin-1: SCENARIO_FAMILY := overhead
longpoll-tgin-1: TRANSPORT := longpoll
longpoll-tgin-1: ROUTE_PATH := /bot1/getUpdates
longpoll-tgin-1: BOT_COUNT := 1
longpoll-tgin-1: clean
	$(call COMPOSE_UP_WAIT,tgin_longpull_one longpull_bot1 bench)
	$(call WAIT_HTTP,$(TGIN_LONGPOLL_API))
	$(call RUN_BENCH,--mode longpoll --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))

longpoll-tgin-scale-2: SCENARIO := longpoll-tgin-scale-2
longpoll-tgin-scale-2: SCENARIO_FAMILY := scale
longpoll-tgin-scale-2: TRANSPORT := longpoll
longpoll-tgin-scale-2: ROUTE_PATH := /bot*/getUpdates
longpoll-tgin-scale-2: BOT_COUNT := 2
longpoll-tgin-scale-2: clean
	$(call COMPOSE_UP_WAIT,tgin_longpull longpull_bot1 longpull_bot2 bench)
	$(call WAIT_HTTP,$(TGIN_LONGPOLL_API))
	$(call RUN_BENCH,--mode longpoll --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))

longpoll-tgin-scale-3: SCENARIO := longpoll-tgin-scale-3
longpoll-tgin-scale-3: SCENARIO_FAMILY := scale
longpoll-tgin-scale-3: TRANSPORT := longpoll
longpoll-tgin-scale-3: ROUTE_PATH := /bot*/getUpdates
longpoll-tgin-scale-3: BOT_COUNT := 3
longpoll-tgin-scale-3: clean
	$(call COMPOSE_UP_WAIT,tgin_longpull longpull_bot1 longpull_bot2 bench)
	$(call WAIT_HTTP,$(TGIN_LONGPOLL_API))
	$(call ADD_LONGPULL_ROUTE,3030,/bot3/getUpdates)
	$(call COMPOSE_UP_WAIT,longpull_bot3)
	$(call RUN_BENCH,--mode longpoll --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))

longpoll-tgin-scale-5: SCENARIO := longpoll-tgin-scale-5
longpoll-tgin-scale-5: SCENARIO_FAMILY := scale
longpoll-tgin-scale-5: TRANSPORT := longpoll
longpoll-tgin-scale-5: ROUTE_PATH := /bot*/getUpdates
longpoll-tgin-scale-5: BOT_COUNT := 5
longpoll-tgin-scale-5: clean
	$(call COMPOSE_UP_WAIT,tgin_longpull longpull_bot1 longpull_bot2 bench)
	$(call WAIT_HTTP,$(TGIN_LONGPOLL_API))
	$(call ADD_LONGPULL_ROUTE,3030,/bot3/getUpdates)
	$(call ADD_LONGPULL_ROUTE,3030,/bot4/getUpdates)
	$(call ADD_LONGPULL_ROUTE,3030,/bot5/getUpdates)
	$(call COMPOSE_UP_WAIT,longpull_bot3 longpull_bot4 longpull_bot5)
	$(call RUN_BENCH,--mode longpoll --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))

longpoll-tgin-scale-10: SCENARIO := longpoll-tgin-scale-10
longpoll-tgin-scale-10: SCENARIO_FAMILY := scale
longpoll-tgin-scale-10: TRANSPORT := longpoll
longpoll-tgin-scale-10: ROUTE_PATH := /bot*/getUpdates
longpoll-tgin-scale-10: BOT_COUNT := 10
longpoll-tgin-scale-10: clean
	$(call COMPOSE_UP_WAIT,tgin_longpull longpull_bot1 longpull_bot2 bench)
	$(call WAIT_HTTP,$(TGIN_LONGPOLL_API))
	$(call ADD_LONGPULL_ROUTE,3030,/bot3/getUpdates)
	$(call ADD_LONGPULL_ROUTE,3030,/bot4/getUpdates)
	$(call ADD_LONGPULL_ROUTE,3030,/bot5/getUpdates)
	$(call ADD_LONGPULL_ROUTE,3030,/bot6/getUpdates)
	$(call ADD_LONGPULL_ROUTE,3030,/bot7/getUpdates)
	$(call ADD_LONGPULL_ROUTE,3030,/bot8/getUpdates)
	$(call ADD_LONGPULL_ROUTE,3030,/bot9/getUpdates)
	$(call ADD_LONGPULL_ROUTE,3030,/bot10/getUpdates)
	$(call COMPOSE_UP_WAIT,longpull_bot3 longpull_bot4 longpull_bot5 longpull_bot6 longpull_bot7 longpull_bot8 longpull_bot9 longpull_bot10)
	$(call RUN_BENCH,--mode longpoll --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))
