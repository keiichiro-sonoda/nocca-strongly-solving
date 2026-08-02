CXX ?= g++
CXXFLAGS ?= -std=c++11 -DNDEBUG -O2 -Wall -Wextra -pedantic

.PHONY: all
all: filter_candidates verify_db

filter_candidates: filter_candidates.cpp zdd.cpp zdd.hpp
	$(CXX) $(CXXFLAGS) filter_candidates.cpp zdd.cpp -o $@

verify_db: verify_db.cpp
	$(CXX) $(CXXFLAGS) verify_db.cpp -o $@
