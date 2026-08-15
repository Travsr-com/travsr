#ifndef WIDGET_H
#define WIDGET_H

namespace app {

class Widget {
public:
    explicit Widget(int size);
    int draw() const;
    int size() const { return size_; }

private:
    int size_;
};

int build_default();

}  // namespace app

#endif
