#include <cstdio>
#include "widget.h"

int main() {
    app::ui::Widget w(10);
    int drawn = w.draw();
    int area = w.area();
    int inl = w.inlineSize();
    int r1 = w.resize(2);
    int r2 = w.resize(3, 4);
    w += 5;
    int def = app::ui::build_default();
    int n = app::ui::Widget::instances();
    int t = app::ui::twice(7);
    app::ui::Box<int> b(3);
    int u = b.unwrap();
    int d = w.describe();
    auto lbl = app::ui::label_of(w);
    std::printf("%d %d %d %d %d %d %d %d %d %d %s\n",
                drawn, area, inl, r1, r2, def, n, t, u, d, lbl.c_str());
    return 0;
}
