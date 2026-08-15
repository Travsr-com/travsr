#include <cstdio>
#include "widget.h"

int main() {
    app::Widget w(10);
    int drawn = w.draw();
    int def = app::build_default();
    std::printf("%d %d\n", drawn, def);
    return 0;
}
