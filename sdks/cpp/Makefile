# Build the prism C++ client, its example, and the codec tests.
#
#   make            # static library  -> libprism.a
#   make example    # example binary  -> quickstart
#   make test       # build & run the header-only codec tests
#   make clean

CXX      ?= c++
CXXFLAGS ?= -std=c++17 -Wall -Wextra -O2
CXXFLAGS += -Iinclude
AR       ?= ar

ifeq ($(OS),Windows_NT)
  LDLIBS += -lws2_32
  EXE := .exe
else
  EXE :=
endif

LIB := libprism.a

.PHONY: all example test clean

all: $(LIB)

$(LIB): src/prism.cpp include/prism/prism.hpp
	$(CXX) $(CXXFLAGS) -c src/prism.cpp -o prism.o
	$(AR) rcs $(LIB) prism.o

example: examples/quickstart.cpp $(LIB)
	$(CXX) $(CXXFLAGS) examples/quickstart.cpp $(LIB) -o quickstart$(EXE) $(LDLIBS)

# The codec lives in the header, so the tests need neither the .cpp nor sockets.
test: tests/test_codec.cpp include/prism/prism.hpp
	$(CXX) $(CXXFLAGS) tests/test_codec.cpp -o test_codec$(EXE)
	./test_codec$(EXE)

clean:
	rm -f prism.o $(LIB) quickstart$(EXE) test_codec$(EXE)
