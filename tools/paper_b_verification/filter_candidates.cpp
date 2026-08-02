#include <array>
#include <cstdint>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <map>
#include <sstream>
#include <stdexcept>
#include <string>
#include <vector>

#include "zdd.hpp"

namespace {

constexpr std::uint64_t kDbShardSize = 9864659952ULL;
constexpr std::uint64_t kExpectedZddPaths = 147969899280ULL;

// Author sample-code-2 object IDs, bottom to top.
constexpr const char* kStacks[15] = {
    ".", "W", "B", "WW", "WB", "BW", "BB", "WWW",
    "WWB", "WBW", "BWW", "WBB", "BWB", "BBW", "BBB",
};

constexpr int kBlackCount[15] = {
    0, 0, 1, 0, 1, 1, 2, 0, 1, 1, 1, 2, 2, 2, 3,
};
constexpr int kWhiteCount[15] = {
    0, 1, 0, 2, 1, 1, 0, 3, 2, 2, 2, 1, 1, 1, 0,
};

std::vector<std::string> split(const std::string& s, char delim) {
    std::vector<std::string> fields;
    std::size_t begin = 0;
    while (true) {
        const std::size_t end = s.find(delim, begin);
        fields.push_back(s.substr(begin, end - begin));
        if (end == std::string::npos) return fields;
        begin = end + 1;
    }
}

int stack_to_objid(const std::string& stack) {
    for (int i = 0; i < 15; ++i) {
        if (stack == kStacks[i]) return i;
    }
    throw std::runtime_error("unknown stack: " + stack);
}

// reachproj writes row 0 (Black/side-to-move's home row) first.  The author
// sample prints/stores Black's goal row first, so reverse the row order while
// keeping columns unchanged.  A horizontal reflection would be equivalent for
// values, but this convention gives one deterministic author database ID.
std::array<int, 30> parse_reachproj_board(const std::string& board) {
    const auto rows = split(board, '/');
    if (rows.size() != 6) throw std::runtime_error("board must have 6 rows");

    std::array<int, 30> objids{};
    for (int rust_row = 0; rust_row < 6; ++rust_row) {
        const auto cells = split(rows[rust_row], '|');
        if (cells.size() != 5) throw std::runtime_error("board row must have 5 cells");
        const int author_row = 5 - rust_row;
        for (int col = 0; col < 5; ++col) {
            objids[author_row * 5 + col] = stack_to_objid(cells[col]);
        }
    }
    return objids;
}

// This is exactly the terminal condition used by construct_zdd(): five cubes
// of each color and at least one visible (top) cube of each color.  All
// reachproj mirror-space boards already conserve five cubes per player, but
// count them again so format/orientation mistakes fail loudly.
bool is_author_zdd_member(const std::array<int, 30>& objids) {
    int black = 0;
    int white = 0;
    int top_black = 0;
    int top_white = 0;
    for (const int id : objids) {
        black += kBlackCount[id];
        white += kWhiteCount[id];
        const std::string stack = kStacks[id];
        if (!stack.empty() && stack.back() == 'B') ++top_black;
        if (!stack.empty() && stack.back() == 'W') ++top_white;
    }
    if (black != 5 || white != 5) {
        std::ostringstream msg;
        msg << "piece count mismatch: B=" << black << " W=" << white;
        throw std::runtime_error(msg.str());
    }
    return top_black != 0 && top_white != 0;
}

std::uint64_t author_id_checked(const std::array<int, 30>& objids) {
    int input[30];
    for (int i = 0; i < 30; ++i) input[i] = objids[i];

    const std::uint64_t id = ZDD::get().compute_id(input);
    if (id >= ZDD::get().get_path_num()) {
        throw std::runtime_error("ZDD ID outside database");
    }

    unsigned char decoded[30];
    ZDD::get().compute_array(id, decoded);
    for (int i = 0; i < 30; ++i) {
        if (decoded[i] != objids[i]) {
            std::ostringstream msg;
            msg << "ZDD ID round-trip mismatch at loc " << i;
            throw std::runtime_error(msg.str());
        }
    }
    return id;
}

std::array<int, 30> mirror_board(const std::array<int, 30>& objids) {
    std::array<int, 30> mirrored{};
    for (int row = 0; row < 6; ++row) {
        for (int col = 0; col < 5; ++col) {
            mirrored[row * 5 + col] = objids[row * 5 + (4 - col)];
        }
    }
    return mirrored;
}

}  // namespace

int main(int argc, char** argv) {
    if (argc != 3) {
        std::cerr << "usage: " << argv[0] << " INPUT.csv OUTPUT.csv\n";
        return 2;
    }

    const std::uint64_t zdd_paths = ZDD::get().get_path_num();
    if (zdd_paths != kExpectedZddPaths) {
        std::cerr << "unexpected author ZDD path count: " << zdd_paths << '\n';
        return 1;
    }

    std::ifstream input(argv[1]);
    if (!input) {
        std::cerr << "cannot open input: " << argv[1] << '\n';
        return 2;
    }
    std::ofstream output(argv[2]);
    if (!output) {
        std::cerr << "cannot open output: " << argv[2] << '\n';
        return 2;
    }

    std::uint64_t n_input = 0;
    std::uint64_t n_member = 0;
    std::map<std::pair<std::string, int>, std::uint64_t> groups;
    bool saw_header = false;
    std::string line;
    std::uint64_t line_no = 0;

    try {
        while (std::getline(input, line)) {
            ++line_no;
            if (line.empty() || line[0] == '#') continue;
            if (!saw_header) {
                if (line != "rank,value,dtm,selfsym,nmoves,try_win,key_hex,board") {
                    throw std::runtime_error("unexpected CSV header");
                }
                output << line
                       << ",paper_id,db_file,db_offset"
                          ",paper_id_mirror,db_file_mirror,db_offset_mirror\n";
                saw_header = true;
                continue;
            }

            const auto fields = split(line, ',');
            if (fields.size() != 8) throw std::runtime_error("expected 8 CSV fields");
            ++n_input;

            const auto objids = parse_reachproj_board(fields[7]);
            if (!is_author_zdd_member(objids)) continue;

            const std::uint64_t id = author_id_checked(objids);
            const std::uint64_t shard = id / kDbShardSize;
            const std::uint64_t offset = id % kDbShardSize;
            const std::uint64_t mirror_id = author_id_checked(mirror_board(objids));
            const std::uint64_t mirror_shard = mirror_id / kDbShardSize;
            const std::uint64_t mirror_offset = mirror_id % kDbShardSize;
            if (fields[3] == "0" && id == mirror_id) {
                throw std::runtime_error("non-self-symmetric row has identical mirror ID");
            }
            const int dtm = std::stoi(fields[2]);
            ++n_member;
            ++groups[{fields[1], dtm}];

            output << line << ',' << id << ",db" << std::setfill('0')
                   << std::setw(2) << shard << ".bin," << std::setfill(' ')
                   << offset << ',' << mirror_id << ",db" << std::setfill('0')
                   << std::setw(2) << mirror_shard << ".bin," << std::setfill(' ')
                   << mirror_offset << '\n';
        }
    } catch (const std::exception& e) {
        std::cerr << argv[1] << ':' << line_no << ": " << e.what() << '\n';
        return 1;
    }

    if (!saw_header) {
        std::cerr << "CSV header not found\n";
        return 1;
    }
    std::cerr << "author ZDD paths=" << zdd_paths << '\n';
    std::cerr << "input=" << n_input << " member=" << n_member
              << " rejected=" << (n_input - n_member) << '\n';
    for (const auto& entry : groups) {
        std::cerr << entry.first.first << "/DTM" << entry.first.second
                  << '=' << entry.second << '\n';
    }
    return 0;
}
