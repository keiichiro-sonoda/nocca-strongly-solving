#include <cassert>

struct Node{
    unsigned char nb;
    unsigned char nw;
    unsigned char nw4;
    unsigned char f;
    unsigned char topb;
    unsigned char topw;
    unsigned long long int num;
    unsigned char visited;
    unsigned char bvisited;
    unsigned char locid;
    unsigned char loc_stateid;
    int lengthmax;
    int lengthmin;
    unsigned long long int lengthsum;
    Node *left; //0-枝
    Node *right; //1-枝
};

class ZDD {
private:
    Node* m_root;
    ZDD() noexcept;
    ZDD(const ZDD&);
    ZDD& operator=(const ZDD&);
    ~ZDD() noexcept;
public:
    static ZDD& get() noexcept;
    void out_info() const noexcept;
    void compute_array(unsigned long long int x, unsigned char array_objid[30]) const noexcept;
    unsigned long long int compute_id(/*unsigned char*/ int array_objid[30]) const noexcept;
    int compute_length(unsigned long long int ) const noexcept;
    unsigned long long int get_path_num() const noexcept {
        assert(m_root->num == 147969899280ULL);
        //return 1000000000;
        //return 147969899280ULL;
        return m_root->num;
    }
};