const LETTERS = ['s', 't', 'e', 'm', '-', 'r', 'e', 'c', 'o', 'r', 'd', 'e', 'r'];

export function Wordmark() {
  return (
    <span className="wmm" aria-label="$ stem-recorder">
      <span className="d">$</span>
      {LETTERS.map((ch, i) => (
        <span key={i} className="l" data-l={ch}>
          {ch}
        </span>
      ))}
    </span>
  );
}
