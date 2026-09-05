require 'set'

module Acme
  class Greeter
    attr_reader :prefix

    def initialize(prefix)
      @prefix = prefix
    end

    def greet(name)
      "#{prefix} #{name}"
    end
  end
end

puts Acme::Greeter.new('hello').greet('world')
