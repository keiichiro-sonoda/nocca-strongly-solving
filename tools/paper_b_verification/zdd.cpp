#include <deque>
#include <cassert>
#include <algorithm>
#include <iostream>
#include "zdd.hpp"

using namespace std;

struct LocInfo {
    char cube[4];
    unsigned char height;
    unsigned char nb;
    unsigned char nw;
    char top ;
    bool equal(const LocInfo &o) const noexcept {
        return (cube[0] == o.cube[0] && cube[1] == o.cube[1] && cube[2] == o.cube[2] && cube[3] == o.cube[3] && height == o.height);
    }
};

constexpr LocInfo tbl_objid2locinfo[16] = {
     { {'.', '.', '.'}, 0, 0, 0, '.'},
     { {'W', '.', '.'}, 1, 0, 1, 'W'},
     { {'B', '.', '.'}, 1, 1, 0, 'B'},
     { {'W', 'W', '.'}, 2, 0, 2, 'W'},
     { {'W', 'B', '.'}, 2, 1, 1, 'B'},
     { {'B', 'W', '.'}, 2, 1, 1, 'W'},
     { {'B', 'B', '.'}, 2, 2, 0, 'B'},
     { {'W', 'W', 'W'}, 3, 0, 3, 'W'},
     { {'W', 'W', 'B'}, 3, 1, 2, 'B'},
     { {'W', 'B', 'W'}, 3, 1, 2, 'W'},
     { {'B', 'W', 'W'}, 3, 1, 2, 'W'},
     { {'W', 'B', 'B'}, 3, 2, 1, 'B'},
     { {'B', 'W', 'B'}, 3, 2, 1, 'B'},
     { {'B', 'B', 'W'}, 3, 2, 1, 'W'},
     { {'B', 'B', 'B'}, 3, 3, 0, 'B'},
     { {'N', 'N', 'N'}, 99, 99, 99, 'E'}
};

static bool IsNextLeaf0(const Node* n, int d, int x){
    assert(n);
    if(x == 1 && n->f == 1) return true;
    if(d == 449){
        if(x == 1 && n->f == 0 && n->nb == 2 && n->nw == 5 && n->topw != 0) return false;
        if(x == 0 && n->f == 1 && n->nb == 5 && n->nw == 5 && n->topb != 0 && n->topw != 0) return false;
        return true;
    }
    if(d % 15 == 14){
        if(x == 0 && n->f == 1) return false;
        if(x == 1 && n->f == 0 && n->nw <= 5 && n->nb <= 2) return false;
        return true;
    }
    if(x == 1 && n->f == 0 && n->nw + tbl_objid2locinfo[d%15].nw <= 5 && n->nb + tbl_objid2locinfo[d%15].nb <= 5) return false;
    if(x == 0) return false;
    return true;
}

static void dfs(Node* n){
    if(n->left->visited != 1) dfs(n->left);
    if(n->right->visited != 1) dfs(n->right);
    n->num = n->left->num + n->right->num;
    n->lengthmax = max(n->left->lengthmax, n->right->lengthmax) + 1;
    n->lengthmin = min(n->left->lengthmin, n->right->lengthmin) + 1;
    n->lengthsum = n->left->lengthsum + n->right->lengthsum + n->num;
    n->visited = 1;
}

static Node* construct_zdd() noexcept {
    Node* root;

    int d = -1;
    deque<Node*> N[450];
    root = new Node;
    Node* l0 = new Node;
    Node* l1 = new Node;
    Node* n;

    root->f = 0;
    root->nb = 0;
    root->nw = 0;
    root->topw = 0;
    root->topb = 0;
    root->num = 0;
    root->bvisited = 0;
    root->loc_stateid = 0;
    root->locid = 0;

    l0->f = 20;
    l0->nb = 20;
    l0->nw = 20;
    l0->left = NULL;
    l0->right = NULL;
    l0->topw = 20;
    l0->topb = 20;
    l0->num = 0;
    l0->visited = 1;
    l0->bvisited = 2;
    l0->lengthmax = -500;
    l0->lengthmin = 500;
    l0->lengthsum = 0;

    l1->f = 30;
    l1->nb = 30;
    l1->nw = 30;
    l1->left = NULL;
    l1->right = NULL;
    l1->topw = 30;
    l1->topb = 30;
    l1->num = 1;
    l1->visited = 1;
    l1->bvisited = 2;
    l1->lengthmax = 0;
    l1->lengthmin = 0;
    l1->lengthsum = 0;

    N[0].push_back(root);
    for(int i = 0; i < 30; i++){
        for(int j = 0; j < 15; j++){
            d++;
            for(size_t k = 0; k < N[d].size(); k++){
                n = N[d][k];
                for(int x = 0; x < 2; x++){
                    if(IsNextLeaf0(n, d, x)){
                        if(x == 0) n->left = l0;
                        else n->right = l0;
                    }else if(d == 449){
                        if(x == 0) n->left = l1;
                        else n->right = l1;
                    }else{
                        Node *c = new Node;
                        if(j == 14) c->locid = i + 1;
                        else c->locid = i;
                        if(j == 14) c->loc_stateid = 0;
                        else c->loc_stateid = j + 1;
                        c->f = 0;
                        if(j != 14 && (n->f == 1 || x == 1)) c->f = 1;
                        c->nb = n->nb;
                        c->nw = n->nw;
                        if(x == 1){
                            c->nw = c->nw + tbl_objid2locinfo[j].nw;
                            c->nb = c->nb + tbl_objid2locinfo[j].nb;
                        }
                        c->topw = n->topw;
                        c->topb = n->topb;
                        if(x == 1 && tbl_objid2locinfo[j].top == 'W'){
                            c->topw = c->topw + 1;
                        }else if(x == 1 && tbl_objid2locinfo[j].top == 'B'){
                            c->topb = c->topb + 1;
                        }
                        c->num = 0;
                        c->visited = 0;
                        c->bvisited = 0;
                        if(N[d+1].size() == 0){
                            N[d+1].push_back(c);
                            if(x == 0) n->left = c;
                            else n->right = c;
                        }else{
                            for(size_t l = 0; l < N[d+1].size(); l++){
                                if(N[d+1][l]->f == c->f && N[d+1][l]->nb == c->nb && N[d+1][l]->nw == c->nw && ((N[d+1][l]->topw == 0 && c->topw == 0) || (N[d+1][l]->topw != 0 && c->topw != 0)) && ((N[d+1][l]->topb == 0 && c->topb == 0) || (N[d+1][l]->topb != 0 && c->topb != 0))){
                                    if(x == 0) n->left = N[d+1][l];
                                    else n->right = N[d+1][l];
                                    delete c;
                                    break;
                                }else if(l == N[d+1].size() - 1){
                                    N[d+1].push_back(c);
                                    if(x == 0) n->left = c;
                                    else n->right = c;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    int end = 1;
    //Remove redundant nodes
    while(end == 1){
        end = 0;
        for(int i = 0; i < 449; i++){
            for(size_t j = 0; j < N[i].size(); j++){
                if(N[i][j]->left->f != 20 && N[i][j]->left->f != 30 && N[i][j]->f != 40){
                    if(N[i][j]->left->right->f == 20){
                        end = 1;
                        N[i][j]->left->f = 40;
                        N[i][j]->left = N[i][j]->left->left;
                    }
                }
                if(N[i][j]->right->f != 20 && N[i][j]->right->f != 30 && N[i][j]->f != 40){
                    if(N[i][j]->right->right->f == 20){
                        end = 1;
                        N[i][j]->right->f = 40;
                        N[i][j]->right = N[i][j]->right->left;
                    }
                }
            }
        }
    }
    for(int i = 0; i < 450; i++){
        for(size_t j = 0; j < N[i].size(); j++){
            if(N[i][j]->f == 40){
                N[i].erase(N[i].begin() + j);
                j--;
            }
        }
    }

    dfs(root);

    return root;
}




ZDD::ZDD() noexcept {
    m_root = construct_zdd();
}
ZDD::~ZDD() noexcept {}

ZDD& ZDD::get() noexcept {
    static ZDD inst;
    return inst;
}

int ZDD::compute_length(unsigned long long int id) const noexcept {
    unsigned long long int x = id;
    int length = 0;
    Node* r = m_root;
    while(1){
        assert(r->f == 30 || r->f == 0 || r->f == 1);
        if(r->f == 30){
            break;
        }else if(r->left->num <= x){
            x -= r->left->num;
            r = r->right;
            length++;
        }else{
            r = r->left;
            length++;
        }
    }
    return length;
}

void ZDD::compute_array(unsigned long long int x, unsigned char array_objid[30]) const noexcept {
    const Node *r = m_root;
    for(int i = 0; i < 30; i++) {
        array_objid[i] = 14;
    }
    while (true) {
        assert(r);
        if(r->f == 30) break;
        assert(r->f != 20);
        assert(r->f == 0 || r->f == 1);
        if(r->left->num <= x) {
            x -= r->left->num;
            array_objid[r->locid] = r->loc_stateid;
            r = r->right;
        } else {
            r = r->left;
        }
    }
}

unsigned long long int ZDD::compute_id(int  array_objid[30]) const noexcept {
    const Node* r = m_root;
    unsigned long long int index = 0;
    while(r->f < 20){
        if(array_objid[r->locid] == r->loc_stateid) {
            index += r->left->num;
            r = r->right;
        } else {
            r = r->left;
        }
    }
    return index;
}


void ZDD::out_info() const noexcept {
    cout << "root->num = " << m_root->num << endl;
    cout << "root->lengthmax = " << m_root->lengthmax << endl;
    cout << "root->lengthmin = " << m_root->lengthmin << endl;
    cout << "root->lengthave = " << (double)m_root->lengthsum / (double)m_root->num << endl;
}