// ===== Scroll Reveal =====
const reveals = document.querySelectorAll('.reveal');
const revealObserver = new IntersectionObserver((entries) => {
  entries.forEach(entry => {
    if (entry.isIntersecting) {
      entry.target.classList.add('visible');
      revealObserver.unobserve(entry.target);
    }
  });
}, { threshold: 0.12, rootMargin: '0px 0px -40px 0px' });
reveals.forEach(el => revealObserver.observe(el));

// ===== Nav scroll effect =====
const nav = document.querySelector('nav');
window.addEventListener('scroll', () => {
  nav.classList.toggle('scrolled', window.scrollY > 40);
}, { passive: true });

// ===== Smooth anchor scrolling =====
const reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)');
document.querySelectorAll('a[href^="#"]').forEach(link => {
  link.addEventListener('click', e => {
    e.preventDefault();
    const target = document.querySelector(link.getAttribute('href'));
    if (target) target.scrollIntoView({ behavior: reduceMotion.matches ? 'auto' : 'smooth', block: 'start' });
  });
});

// ===== Staggered card reveals =====
document.querySelectorAll('.grid-3, .grid-2').forEach(grid => {
  const cards = grid.querySelectorAll('.card');
  const observer = new IntersectionObserver((entries) => {
    entries.forEach(entry => {
      if (entry.isIntersecting) {
        cards.forEach((card, i) => {
          card.style.transitionDelay = `${i * 80}ms`;
          card.classList.add('visible');
        });
        observer.unobserve(entry.target);
      }
    });
  }, { threshold: 0.1 });
  cards.forEach(c => { c.classList.add('reveal'); });
  observer.observe(grid);
});

// ===== GitHub Stars & Forks =====
fetch('https://api.github.com/repos/Qanata/HCP')
  .then(r => r.json())
  .then(data => {
    const stars = document.getElementById('star-count');
    const forks = document.getElementById('fork-count');
    if (stars && data.stargazers_count !== undefined) stars.textContent = data.stargazers_count;
    if (forks && data.forks_count !== undefined) forks.textContent = data.forks_count;
  })
  .catch(() => { });
