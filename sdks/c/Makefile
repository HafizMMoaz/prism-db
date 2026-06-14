# Build the prism C client, its example, and the codec tests.
#
#   make            # static library  -> libprism.a
#   make example    # example binary  -> quickstart
#   make test       # build & run the no-server codec tests
#   make clean

CC      ?= cc
CFLAGS  ?= -std=c11 -Wall -Wextra -O2
CFLAGS  += -Iinclude
AR      ?= ar

# Winsock on Windows; nothing extra on POSIX.
ifeq ($(OS),Windows_NT)
  LDLIBS += -lws2_32
  EXE := .exe
else
  EXE :=
endif

LIB := libprism.a

.PHONY: all example test clean

all: $(LIB)

$(LIB): src/prism.c include/prism.h
	$(CC) $(CFLAGS) -c src/prism.c -o prism.o
	$(AR) rcs $(LIB) prism.o

example: examples/quickstart.c $(LIB)
	$(CC) $(CFLAGS) examples/quickstart.c $(LIB) -o quickstart$(EXE) $(LDLIBS)

test: tests/test_codec.c src/prism.c include/prism.h
	$(CC) $(CFLAGS) tests/test_codec.c -o test_codec$(EXE) $(LDLIBS)
	./test_codec$(EXE)

clean:
	rm -f prism.o $(LIB) quickstart$(EXE) test_codec$(EXE)
