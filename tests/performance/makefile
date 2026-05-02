RPS ?= 100
DURATION ?= 10
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
	docker compose up -d webhook_bot1 bench
	@sleep 10
	$(call RUN_BENCH,--target http://10.5.0.11:8080/webhook --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))
	@sleep 60

webhook-tgin-1: SCENARIO := webhook-tgin-1
webhook-tgin-1: SCENARIO_FAMILY := overhead
webhook-tgin-1: TRANSPORT := webhook
webhook-tgin-1: ROUTE_PATH := /webhook
webhook-tgin-1: BOT_COUNT := 1
webhook-tgin-1: clean
	docker compose up -d tgin_webhook_one webhook_bot1 bench
	@sleep 10
	$(call RUN_BENCH,--target http://10.5.0.2:3000/webhook --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))
	@sleep 60

webhook-tgin-scale-2: SCENARIO := webhook-tgin-scale-2
webhook-tgin-scale-2: SCENARIO_FAMILY := scale
webhook-tgin-scale-2: TRANSPORT := webhook
webhook-tgin-scale-2: ROUTE_PATH := /webhook
webhook-tgin-scale-2: BOT_COUNT := 2
webhook-tgin-scale-2: clean
	docker compose up -d tgin_webhook webhook_bot1 webhook_bot2 bench
	@sleep 10
	$(call RUN_BENCH,--target http://10.5.0.2:3000/webhook --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))
	@sleep 60

webhook-tgin-scale-3: SCENARIO := webhook-tgin-scale-3
webhook-tgin-scale-3: SCENARIO_FAMILY := scale
webhook-tgin-scale-3: TRANSPORT := webhook
webhook-tgin-scale-3: ROUTE_PATH := /webhook
webhook-tgin-scale-3: BOT_COUNT := 3
webhook-tgin-scale-3: clean
	docker compose up -d tgin_webhook webhook_bot1 webhook_bot2 bench webhook_bot3
	@sleep 10
	curl --request POST http://localhost:3000/api/route --data '{ "type": "Webhook", "url": "http://10.5.0.13:8080/webhook"}' --header "Content-Type: application/json"
	$(call RUN_BENCH,--target http://10.5.0.2:3000/webhook --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))
	@sleep 60

webhook-tgin-scale-5: SCENARIO := webhook-tgin-scale-5
webhook-tgin-scale-5: SCENARIO_FAMILY := scale
webhook-tgin-scale-5: TRANSPORT := webhook
webhook-tgin-scale-5: ROUTE_PATH := /webhook
webhook-tgin-scale-5: BOT_COUNT := 5
webhook-tgin-scale-5: clean
	docker compose up -d tgin_webhook webhook_bot1 webhook_bot2 bench webhook_bot3 webhook_bot4 webhook_bot5
	@sleep 10
	curl --request POST http://localhost:3000/api/route --data '{ "type": "Webhook", "url": "http://10.5.0.13:8080/webhook"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3000/api/route --data '{ "type": "Webhook", "url": "http://10.5.0.14:8080/webhook"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3000/api/route --data '{ "type": "Webhook", "url": "http://10.5.0.15:8080/webhook"}' --header "Content-Type: application/json"
	$(call RUN_BENCH,--target http://10.5.0.2:3000/webhook --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))
	@sleep 60

webhook-tgin-scale-10: SCENARIO := webhook-tgin-scale-10
webhook-tgin-scale-10: SCENARIO_FAMILY := scale
webhook-tgin-scale-10: TRANSPORT := webhook
webhook-tgin-scale-10: ROUTE_PATH := /webhook
webhook-tgin-scale-10: BOT_COUNT := 10
webhook-tgin-scale-10: clean
	docker compose up -d tgin_webhook webhook_bot1 webhook_bot2 bench webhook_bot3 webhook_bot4 webhook_bot5 webhook_bot6 webhook_bot7 webhook_bot8 webhook_bot9 webhook_bot10
	@sleep 10
	curl --request POST http://localhost:3000/api/route --data '{ "type": "Webhook", "url": "http://10.5.0.13:8080/webhook"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3000/api/route --data '{ "type": "Webhook", "url": "http://10.5.0.14:8080/webhook"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3000/api/route --data '{ "type": "Webhook", "url": "http://10.5.0.15:8080/webhook"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3000/api/route --data '{ "type": "Webhook", "url": "http://10.5.0.16:8080/webhook"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3000/api/route --data '{ "type": "Webhook", "url": "http://10.5.0.17:8080/webhook"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3000/api/route --data '{ "type": "Webhook", "url": "http://10.5.0.18:8080/webhook"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3000/api/route --data '{ "type": "Webhook", "url": "http://10.5.0.19:8080/webhook"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3000/api/route --data '{ "type": "Webhook", "url": "http://10.5.0.199:8080/webhook"}' --header "Content-Type: application/json"
	$(call RUN_BENCH,--target http://10.5.0.2:3000/webhook --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))
	@sleep 60

longpoll-direct-1: SCENARIO := longpoll-direct-1
longpoll-direct-1: SCENARIO_FAMILY := overhead
longpoll-direct-1: TRANSPORT := longpoll
longpoll-direct-1: ROUTE_PATH := /bot123:test/getUpdates
longpoll-direct-1: BOT_COUNT := 1
longpoll-direct-1: clean
	docker compose up -d longpull_bot_direct bench
	@sleep 10
	$(call RUN_BENCH,--mode longpoll --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))
	@sleep 60

longpoll-tgin-1: SCENARIO := longpoll-tgin-1
longpoll-tgin-1: SCENARIO_FAMILY := overhead
longpoll-tgin-1: TRANSPORT := longpoll
longpoll-tgin-1: ROUTE_PATH := /bot1/getUpdates
longpoll-tgin-1: BOT_COUNT := 1
longpoll-tgin-1: clean
	docker compose up -d tgin_longpull_one longpull_bot1 bench
	@echo "Waiting for tgin API on :3030 ..."
	@for i in $$(seq 1 30); do \
		curl -sf http://localhost:3030/api/routes > /dev/null 2>&1 && break; \
		sleep 1; \
	done
	@sleep 2
	$(call RUN_BENCH,--mode longpoll --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))
	@sleep 60

longpoll-tgin-scale-2: SCENARIO := longpoll-tgin-scale-2
longpoll-tgin-scale-2: SCENARIO_FAMILY := scale
longpoll-tgin-scale-2: TRANSPORT := longpoll
longpoll-tgin-scale-2: ROUTE_PATH := /bot*/getUpdates
longpoll-tgin-scale-2: BOT_COUNT := 2
longpoll-tgin-scale-2: clean
	docker compose up -d tgin_longpull longpull_bot1 longpull_bot2 bench
	@echo "Waiting for tgin API on :3030 ..."
	@for i in $$(seq 1 30); do \
		curl -sf http://localhost:3030/api/routes > /dev/null 2>&1 && break; \
		sleep 1; \
	done
	@sleep 2
	$(call RUN_BENCH,--mode longpoll --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))
	@sleep 60

longpoll-tgin-scale-3: SCENARIO := longpoll-tgin-scale-3
longpoll-tgin-scale-3: SCENARIO_FAMILY := scale
longpoll-tgin-scale-3: TRANSPORT := longpoll
longpoll-tgin-scale-3: ROUTE_PATH := /bot*/getUpdates
longpoll-tgin-scale-3: BOT_COUNT := 3
longpoll-tgin-scale-3: clean
	docker compose up -d tgin_longpull longpull_bot1 longpull_bot2 bench
	@echo "Waiting for tgin API on :3030 ..."
	@for i in $$(seq 1 30); do \
		curl -sf http://localhost:3030/api/routes > /dev/null 2>&1 && break; \
		sleep 1; \
	done
	@sleep 2
	curl --request POST http://localhost:3030/api/route --data '{ "type": "Longpull", "path": "/bot3/getUpdates"}' --header "Content-Type: application/json"
	docker compose up -d longpull_bot3
	@sleep 3
	$(call RUN_BENCH,--mode longpoll --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))
	@sleep 60

longpoll-tgin-scale-5: SCENARIO := longpoll-tgin-scale-5
longpoll-tgin-scale-5: SCENARIO_FAMILY := scale
longpoll-tgin-scale-5: TRANSPORT := longpoll
longpoll-tgin-scale-5: ROUTE_PATH := /bot*/getUpdates
longpoll-tgin-scale-5: BOT_COUNT := 5
longpoll-tgin-scale-5: clean
	docker compose up -d tgin_longpull longpull_bot1 longpull_bot2 bench
	@echo "Waiting for tgin API on :3030 ..."
	@for i in $$(seq 1 30); do \
		curl -sf http://localhost:3030/api/routes > /dev/null 2>&1 && break; \
		sleep 1; \
	done
	@sleep 2
	curl --request POST http://localhost:3030/api/route --data '{ "type": "Longpull", "path": "/bot3/getUpdates"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3030/api/route --data '{ "type": "Longpull", "path": "/bot4/getUpdates"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3030/api/route --data '{ "type": "Longpull", "path": "/bot5/getUpdates"}' --header "Content-Type: application/json"
	docker compose up -d longpull_bot3 longpull_bot4 longpull_bot5
	@sleep 3
	$(call RUN_BENCH,--mode longpoll --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))
	@sleep 60

longpoll-tgin-scale-10: SCENARIO := longpoll-tgin-scale-10
longpoll-tgin-scale-10: SCENARIO_FAMILY := scale
longpoll-tgin-scale-10: TRANSPORT := longpoll
longpoll-tgin-scale-10: ROUTE_PATH := /bot*/getUpdates
longpoll-tgin-scale-10: BOT_COUNT := 10
longpoll-tgin-scale-10: clean
	docker compose up -d tgin_longpull longpull_bot1 longpull_bot2 bench
	@echo "Waiting for tgin API on :3030 ..."
	@for i in $$(seq 1 30); do \
		curl -sf http://localhost:3030/api/routes > /dev/null 2>&1 && break; \
		sleep 1; \
	done
	@sleep 2
	curl --request POST http://localhost:3030/api/route --data '{ "type": "Longpull", "path": "/bot3/getUpdates"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3030/api/route --data '{ "type": "Longpull", "path": "/bot4/getUpdates"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3030/api/route --data '{ "type": "Longpull", "path": "/bot5/getUpdates"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3030/api/route --data '{ "type": "Longpull", "path": "/bot6/getUpdates"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3030/api/route --data '{ "type": "Longpull", "path": "/bot7/getUpdates"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3030/api/route --data '{ "type": "Longpull", "path": "/bot8/getUpdates"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3030/api/route --data '{ "type": "Longpull", "path": "/bot9/getUpdates"}' --header "Content-Type: application/json"
	curl --request POST http://localhost:3030/api/route --data '{ "type": "Longpull", "path": "/bot10/getUpdates"}' --header "Content-Type: application/json"
	docker compose up -d longpull_bot3 longpull_bot4 longpull_bot5 longpull_bot6 longpull_bot7 longpull_bot8 longpull_bot9 longpull_bot10
	@sleep 3
	$(call RUN_BENCH,--mode longpoll --port $(BENCH_PORT) --rps $(RPS) --duration $(DURATION))
	@sleep 60
