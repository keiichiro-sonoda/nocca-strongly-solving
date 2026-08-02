#include <cstdint>
#include <fstream>
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

namespace {

constexpr std::uint64_t kShardSize = 9864659952ULL;

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

unsigned int read_byte(std::ifstream& db, std::uint64_t offset) {
    db.clear();
    db.seekg(static_cast<std::streamoff>(offset), std::ios::beg);
    unsigned char byte = 0;
    db.read(reinterpret_cast<char*>(&byte), 1);
    if (!db) throw std::runtime_error("database read failed");
    return byte;
}

std::string basename(const std::string& path) {
    const std::size_t slash = path.find_last_of("/\\");
    return slash == std::string::npos ? path : path.substr(slash + 1);
}

unsigned int shard_id(const std::string& name) {
    if (name.size() != 8 || name.substr(0, 2) != "db" ||
        name.substr(4) != ".bin") {
        throw std::runtime_error("database name must be dbNN.bin: " + name);
    }
    return static_cast<unsigned int>(std::stoul(name.substr(2, 2)));
}

}  // namespace

int main(int argc, char** argv) {
    if (argc != 3) {
        std::cerr << "usage: " << argv[0] << " candidates_zdd.csv dbNN.bin\n";
        return 2;
    }

    const std::string db_name = basename(argv[2]);
    unsigned int shard = 0;
    try {
        shard = shard_id(db_name);
    } catch (const std::exception& e) {
        std::cerr << e.what() << '\n';
        return 2;
    }
    std::ifstream csv(argv[1]);
    std::ifstream db(argv[2], std::ios::binary);
    if (!csv || !db) {
        std::cerr << "failed to open input\n";
        return 2;
    }
    db.seekg(0, std::ios::end);
    const std::uint64_t size = static_cast<std::uint64_t>(db.tellg());
    if (size != kShardSize) {
        std::cerr << "unexpected " << db_name << " size: " << size << '\n';
        return 1;
    }

    std::string line;
    std::uint64_t checked = 0;
    std::uint64_t mismatches = 0;
    std::cout << "rank,value,expected_dtm,orientation,paper_id,db_offset,db_byte,result\n";
    while (std::getline(csv, line)) {
        if (line.empty() || line.rfind("rank,", 0) == 0) continue;
        const auto f = split(line, ',');
        if (f.size() != 14) {
            std::cerr << "unexpected CSV field count\n";
            return 1;
        }
        const unsigned int expected = static_cast<unsigned int>(std::stoul(f[2]));
        for (const int base : {8, 11}) {
            if (f[base + 1] != db_name) continue;
            const std::uint64_t id = std::stoull(f[base]);
            const std::uint64_t csv_offset = std::stoull(f[base + 2]);
            if (id / kShardSize != shard || id % kShardSize != csv_offset) {
                std::cerr << "ID/shard/offset mismatch for " << id << '\n';
                return 1;
            }
            const unsigned int actual = read_byte(db, csv_offset);
            const bool ok = actual == expected;
            ++checked;
            if (!ok) ++mismatches;
            std::cout << f[0] << ',' << f[1] << ',' << f[2] << ','
                      << (base == 8 ? "primary" : "mirror") << ',' << id << ','
                      << csv_offset << ',' << actual << ',' << (ok ? "OK" : "MISMATCH")
                      << '\n';
        }
    }
    std::cerr << db_name << ": checked=" << checked << " mismatches=" << mismatches
              << '\n';
    if (checked == 0) {
        std::cerr << "no candidate IDs belong to " << db_name << '\n';
        return 1;
    }
    return mismatches == 0 ? 0 : 1;
}
